/*
Copyright 2022 The Kuasar Authors.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use std::{
    fs::OpenOptions,
    os::unix::prelude::{IntoRawFd, RawFd},
    process::exit,
    sync::Arc,
};

use cgroups_rs::{Cgroup, CgroupPid};
use containerd_shim::{
    api::{CreateTaskRequest, ExecProcessRequest, Status},
    asynchronous::{
        container::{ContainerFactory, ContainerTemplate, ProcessFactory},
        monitor::{monitor_subscribe, monitor_unsubscribe},
        processes::{ProcessLifecycle, ProcessTemplate},
        task::TaskService,
        util::{mkdir, mount_rootfs, read_spec},
    },
    error::Error,
    io::Stdio,
    monitor::{Subject, Topic},
    other, other_error,
    processes::Process,
    protos::{cgroups::metrics::Metrics, shim::oci::Options, types::task::ProcessInfo},
    ExitSignal,
};
use log::debug;
use nix::{
    errno::Errno,
    fcntl::OFlag,
    sched::{setns, CloneFlags},
    sys::{signal::kill, stat::Mode},
    unistd::{dup2, fork, ForkResult, Pid},
};
use oci_spec::runtime::Spec;
use wasmedge_sdk::{
    config::{CommonConfigOptions, ConfigBuilder, HostRegistrationConfigOptions},
    error::WasmEdgeError,
    params,
    plugin::PluginManager,
    wasi::WasiInstance,
    Vm, VmBuilder,
};

use crate::utils::{get_args, get_cgroup_path, get_envs, get_preopens, get_rootfs};

pub type ExecProcess = ProcessTemplate<WasmEdgeExecLifecycle>;
pub type InitProcess = ProcessTemplate<WasmEdgeInitLifecycle>;

pub type WasmEdgeContainer = ContainerTemplate<InitProcess, ExecProcess, ExecFactory>;

pub struct ExecFactory {}

pub struct WasmEdgeExecLifecycle {}

pub struct WasmEdgeInitLifecycle {
    _opts: Options,
    _bundle: String,
    spec: Spec,
    prototype_vm: Vm,
    netns: String,
    _exit_signal: Arc<ExitSignal>,
}

pub struct WasmEdgeContainerFactory {
    prototype_vm: Vm,
    pub(crate) netns: String,
}

impl Default for WasmEdgeContainerFactory {
    fn default() -> Self {
        PluginManager::load(None).unwrap();
        let mut host_options = HostRegistrationConfigOptions::default();
        host_options = host_options.wasi(true);
        let config = ConfigBuilder::new(CommonConfigOptions::default())
            .with_host_registration_config(host_options)
            .build()
            .unwrap();
        let vm = VmBuilder::new()
            .with_config(config)
            .build()
            .map_err(anyhow::Error::msg)
            .unwrap();
        Self {
            prototype_vm: vm,
            netns: "".to_string(),
        }
    }
}

#[async_trait::async_trait]
impl ContainerFactory<WasmEdgeContainer> for WasmEdgeContainerFactory {
    async fn create(
        &self,
        _ns: &str,
        req: &CreateTaskRequest,
    ) -> containerd_shim::Result<WasmEdgeContainer> {
        let mut spec: Spec = read_spec(req.bundle()).await?;
        spec.canonicalize_rootfs(req.bundle())
            .map_err(|e| Error::InvalidArgument(format!("could not canonicalize rootfs: {e}")))?;
        let rootfs = get_rootfs(&spec).ok_or_else(|| {
            Error::InvalidArgument("rootfs is not set in runtime spec".to_string())
        })?;
        mkdir(&rootfs, 0o711).await?;
        for m in req.rootfs() {
            mount_rootfs(m, &rootfs).await?
        }
        let stdio = Stdio::new(req.stdin(), req.stdout(), req.stderr(), req.terminal);
        let exit_signal = Arc::new(Default::default());
        let netns = self.netns.clone();
        let init_process = InitProcess::new(
            req.id(),
            stdio,
            WasmEdgeInitLifecycle {
                _opts: Default::default(),
                _bundle: req.bundle.to_string(),
                _exit_signal: exit_signal,
                spec,
                prototype_vm: self.prototype_vm.clone(),
                netns,
            },
        );
        Ok(WasmEdgeContainer {
            id: req.id.to_string(),
            bundle: req.id.to_string(),
            init: init_process,
            process_factory: ExecFactory {},
            processes: Default::default(),
        })
    }

    async fn cleanup(&self, _ns: &str, _c: &WasmEdgeContainer) -> containerd_shim::Result<()> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl ProcessLifecycle<InitProcess> for WasmEdgeInitLifecycle {
    async fn start(&self, p: &mut InitProcess) -> containerd_shim::Result<()> {
        let spec = &p.lifecycle.spec;
        // Allow vm to be mutable since we change it in wasmedge_wasi_nn feature
        #[allow(unused_mut)]
        #[allow(unused_assignments)]
        let mut vm = p.lifecycle.prototype_vm.clone();
        let args = get_args(spec);
        let envs = get_envs(spec);
        let rootfs = get_rootfs(spec).ok_or_else(|| {
            Error::InvalidArgument("rootfs is not set in runtime spec".to_string())
        })?;
        let mut preopens = vec![format!("/:{}", rootfs)];
        preopens.append(&mut get_preopens(spec));

        debug!(
            "start wasm with args: {:?}, envs: {:?}, preopens: {:?}",
            args, envs, preopens
        );
        match unsafe {
            fork().map_err(other_error!(
                e,
                format!("failed to fork process for {}", p.id)
            ))?
        } {
            ForkResult::Parent { child } => {
                let init_pid = child.as_raw();
                p.state = Status::RUNNING;
                p.pid = init_pid;
            }
            ForkResult::Child => {
                if let Some(cgroup_path) = get_cgroup_path(spec) {
                    // Add child process to Cgroup
                    Cgroup::new(
                        cgroups_rs::hierarchies::auto(),
                        cgroup_path.trim_start_matches('/'),
                    )
                    .and_then(|cgroup| cgroup.add_task(CgroupPid::from(std::process::id() as u64)))
                    .map_err(other_error!(
                        e,
                        format!("failed to add task to cgroup: {}", cgroup_path)
                    ))?;
                }
                // Only create new VM instance on wasmedge_wasi_nn feature
                #[cfg(all(
                    target_os = "linux",
                    feature = "wasmedge_wasi_nn",
                    target_arch = "x86_64"
                ))]
                {
                    const NN_PRELOAD_KEY: &str = "io.kuasar.wasm.nn_preload";
                    if let Some(process) = p.lifecycle.spec.process() {
                        if let Some(env) = process.env() {
                            if let Some(v) =
                                env.iter().find(|k| k.contains(&NN_PRELOAD_KEY.to_string()))
                            {
                                if let Some(nn_preload) =
                                    v.strip_prefix::<&str>(format!("{}=", NN_PRELOAD_KEY).as_ref())
                                {
                                    log::info!("found nn_pre_load: {}", nn_preload);
                                    if let Some(rootfs) = spec.root().as_ref() {
                                        pre_load_with_new_rootfs(nn_preload, rootfs.path())
                                            .unwrap();
                                    }
                                }
                            }
                        }
                    }

                    let host_options = HostRegistrationConfigOptions::default().wasi(true);
                    let config = ConfigBuilder::new(CommonConfigOptions::default())
                        .with_host_registration_config(host_options)
                        .build()
                        .map_err(other_error!(e, "generate default wasmedge config"))?;

                    vm = VmBuilder::new()
                        .with_config(config)
                        .with_plugin_wasi_nn()
                        .with_plugin("wasi_logging", None)
                        .build()
                        .unwrap();
                }
                match run_wasi_func(vm, args, envs, preopens, p) {
                    Ok(_) => exit(0),
                    // TODO add a pipe? to return detailed error message
                    Err(e) => exit(e.to_exit_code()),
                }
            }
        }
        Ok(())
    }

    async fn kill(
        &self,
        p: &mut InitProcess,
        signal: u32,
        _all: bool,
    ) -> containerd_shim::Result<()> {
        debug!("start kill process {}", p.pid);
        if p.state == Status::RUNNING && p.pid > 0 {
            debug!("kill process {}", p.pid);
            kill(
                Pid::from_raw(p.pid),
                nix::sys::signal::Signal::try_from(signal as i32).unwrap(),
            )
            .map_err(other_error!(e, "failed to kill process"))?;
        }
        Ok(())
    }

    async fn delete(&self, p: &mut InitProcess) -> containerd_shim::Result<()> {
        if let Some(cgroup_path) = get_cgroup_path(&p.lifecycle.spec) {
            // Add child process to Cgroup
            Cgroup::load(
                cgroups_rs::hierarchies::auto(),
                cgroup_path.trim_start_matches('/'),
            )
            .delete()
            .map_err(other_error!(
                e,
                format!("failed to delete cgroup: {}", cgroup_path)
            ))?;
        }
        Ok(())
    }

    async fn update(
        &self,
        _p: &mut InitProcess,
        _resources: &oci_spec::runtime::LinuxResources,
    ) -> containerd_shim::Result<()> {
        Err(Error::Unimplemented(
            "exec not supported for wasm containers".to_string(),
        ))
    }

    async fn stats(&self, p: &InitProcess) -> containerd_shim::Result<Metrics> {
        debug!("get stats of process {}", p.pid);
        if p.pid <= 0 {
            return Err(other!(
                "failed to collect metrics because init process is {}",
                p.pid
            ));
        }
        // Because Wasm Applications execute the instructions inside the host Wasm
        // Runtime, we should read the metrics from Cgroup for the CPU, memory,
        // and filesystem usage.
        containerd_shim::cgroup::collect_metrics(p.pid as u32)
    }

    async fn ps(&self, p: &InitProcess) -> containerd_shim::Result<Vec<ProcessInfo>> {
        let mut process_info = ProcessInfo::new();
        process_info.pid = p.pid as u32;
        return Ok(vec![process_info]);
    }
}

#[async_trait::async_trait]
impl ProcessLifecycle<ExecProcess> for WasmEdgeExecLifecycle {
    async fn start(&self, _p: &mut ExecProcess) -> containerd_shim::Result<()> {
        Err(Error::Unimplemented(
            "exec not supported for wasm containers".to_string(),
        ))
    }

    async fn kill(
        &self,
        _p: &mut ExecProcess,
        _signal: u32,
        _all: bool,
    ) -> containerd_shim::Result<()> {
        Err(Error::Unimplemented(
            "exec not supported for wasm containers".to_string(),
        ))
    }

    async fn delete(&self, _p: &mut ExecProcess) -> containerd_shim::Result<()> {
        Ok(())
    }

    async fn update(
        &self,
        _p: &mut ExecProcess,
        _resources: &oci_spec::runtime::LinuxResources,
    ) -> containerd_shim::Result<()> {
        Err(Error::Unimplemented(
            "exec not supported for wasm containers".to_string(),
        ))
    }

    async fn stats(&self, _p: &ExecProcess) -> containerd_shim::Result<Metrics> {
        Err(Error::Unimplemented(
            "exec not supported for wasm containers".to_string(),
        ))
    }

    async fn ps(&self, _p: &ExecProcess) -> containerd_shim::Result<Vec<ProcessInfo>> {
        Err(Error::Unimplemented(
            "exec not supported for wasm containers".to_string(),
        ))
    }
}

#[async_trait::async_trait]
impl ProcessFactory<ExecProcess> for ExecFactory {
    async fn create(&self, _req: &ExecProcessRequest) -> containerd_shim::Result<ExecProcess> {
        Err(Error::Unimplemented(
            "exec not supported for wasm containers".to_string(),
        ))
    }
}

pub fn maybe_open_stdio(path: &str) -> Result<Option<RawFd>, std::io::Error> {
    if path.is_empty() {
        return Ok(None);
    }

    match OpenOptions::new().read(true).write(true).open(path) {
        Ok(f) => Ok(Some(f.into_raw_fd())),
        Err(err) => match err.kind() {
            std::io::ErrorKind::NotFound => Ok(None),
            _ => Err(err),
        },
    }
}

pub enum RunError {
    WasmEdge(Box<WasmEdgeError>),
    IO(std::io::Error),
    NoRootInSpec,
    NoModule,
    Sys(Errno),
}

impl RunError {
    pub fn to_exit_code(&self) -> i32 {
        match &self {
            RunError::WasmEdge(_e) => -100,
            RunError::IO(_e) => -101,
            RunError::NoRootInSpec => -102,
            RunError::NoModule => -103,
            RunError::Sys(e) => -(*e as i32),
        }
    }
}

fn run_wasi_func(
    mut vm: Vm,
    args: Vec<String>,
    envs: Vec<String>,
    preopens: Vec<String>,
    p: &InitProcess,
) -> Result<(), RunError> {
    let netns = &*p.lifecycle.netns;
    if !netns.is_empty() {
        let netns_fd =
            nix::fcntl::open(netns, OFlag::O_CLOEXEC, Mode::empty()).map_err(RunError::Sys)?;
        setns(netns_fd, CloneFlags::CLONE_NEWNET).map_err(RunError::Sys)?;
    }
    let wasi_instance: &mut WasiInstance = vm.wasi_module_mut().ok_or(RunError::NoModule)?;
    wasi_instance.initialize(
        Some(args.iter().map(|s| s as &str).collect()),
        Some(envs.iter().map(|s| s as &str).collect()),
        Some(preopens.iter().map(|s| s as &str).collect()),
    );
    let mut cmd = args[0].clone();
    let stripped = args[0].strip_prefix(std::path::MAIN_SEPARATOR);
    if let Some(stripped_cmd) = stripped {
        cmd = stripped_cmd.to_string()
    }
    let stdio = p.stdio.clone();

    let rootfs = p
        .lifecycle
        .spec
        .root()
        .as_ref()
        .ok_or(RunError::NoRootInSpec)?
        .path();
    let mod_path = rootfs.join(cmd);
    let vm = vm
        .register_module_from_file("main", mod_path)
        .map_err(RunError::WasmEdge)?;

    if let Some(stdin) = maybe_open_stdio(&stdio.stdin).map_err(RunError::IO)? {
        dup2(stdin, 0).map_err(RunError::Sys)?;
    }
    if let Some(stdin) = maybe_open_stdio(&stdio.stdout).map_err(RunError::IO)? {
        dup2(stdin, 1).map_err(RunError::Sys)?;
    }
    if let Some(stdin) = maybe_open_stdio(&stdio.stderr).map_err(RunError::IO)? {
        dup2(stdin, 2).map_err(RunError::Sys)?;
    }
    vm.run_func(Some("main"), "_start", params!())
        .map_err(RunError::WasmEdge)?;
    Ok(())
}

// any wasm runtime implementation should implement this function
pub async fn process_exits<F>(task: &TaskService<F, WasmEdgeContainer>) {
    let containers = task.containers.clone();
    let exit_signal = task.exit.clone();
    let mut s = monitor_subscribe(Topic::Pid)
        .await
        .expect("monitor subscribe failed");
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = exit_signal.wait() => {
                    debug!("sandbox exit, should break");
                    monitor_unsubscribe(s.id).await.unwrap_or_default();
                    return;
                },
                res = s.rx.recv() => {
                    if let Some(e) = res {
                        if let Subject::Pid(pid) = e.subject {
                            debug!("receive exit event: {}", &e);
                            let exit_code = e.exit_code;
                            for (_k, cont) in containers.lock().await.iter_mut() {
                                // pid belongs to container init process
                                if cont.init.pid == pid {
                                    // set exit for init process
                                    cont.init.set_exited(exit_code).await;
                                    break;
                                }

                                // pid belongs to container common process
                                for (_exec_id, p) in cont.processes.iter_mut() {
                                    // set exit for exec process
                                    if p.pid == pid {
                                        p.set_exited(exit_code).await;
                                        break;
                                    }
                                }
                            }
                        }
                    } else {
                        monitor_unsubscribe(s.id).await.unwrap_or_default();
                        return;
                    }
                }
            }
        }
    });
}

#[cfg(all(
    target_os = "linux",
    feature = "wasmedge_wasi_nn",
    target_arch = "x86_64"
))]
fn pre_load_with_new_rootfs(
    preload: &str,
    rootfs: &std::path::PathBuf,
) -> Result<(), WasmEdgeError> {
    use wasmedge_sdk::plugin::{ExecutionTarget, GraphEncoding};
    let nn_preload: Vec<&str> = preload.split(':').collect();
    if nn_preload.len() != 4 {
        return Err(WasmEdgeError::Operation(format!(
            "Failed to convert to NNPreload value. Invalid preload string: {}. The correct format is: 'alias:backend:target:path'",
            preload
        )));
    }
    let (alias, backend, target, path) = (
        nn_preload[0].to_string(),
        nn_preload[1]
            .parse::<GraphEncoding>()
            .map_err(|err| WasmEdgeError::Operation(err.to_string()))?,
        nn_preload[2]
            .parse::<ExecutionTarget>()
            .map_err(|err| WasmEdgeError::Operation(err.to_string()))?,
        std::path::Path::new(rootfs).join(nn_preload[3]),
    );
    PluginManager::nn_preload(vec![wasmedge_sdk::plugin::NNPreload::new(
        alias, backend, target, path,
    )]);
    Ok(())
}
