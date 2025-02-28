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
    ops::Add,
    process::Stdio,
    sync::Arc,
    time::{Duration, SystemTime},
};

use async_trait::async_trait;
use containerd_sandbox::PodSandboxConfig;
use containerd_shim::{
    error::Result,
    io_error, other, other_error,
    protos::{protobuf::MessageDyn, topics::TASK_OOM_EVENT_TOPIC},
    util::convert_to_any,
    Error, TtrpcContext, TtrpcResult,
};
use log::debug;
use nix::{
    sys::time::{TimeSpec, TimeValLike},
    time::{clock_gettime, clock_settime, ClockId},
};
use tokio::{
    io::AsyncWriteExt,
    sync::{mpsc::Receiver, Mutex},
};
use vmm_common::{
    api,
    api::{
        empty::Empty,
        events::Envelope,
        sandbox::{
            CheckRequest, ExecVMProcessRequest, ExecVMProcessResponse, SetupSandboxRequest,
            SyncClockPacket, UpdateInterfacesRequest, UpdateRoutesRequest,
        },
    },
};

use crate::{netlink::Handle, sandbox::setup_sandbox, NAMESPACE};

pub struct SandboxService {
    pub namespace: String,
    pub handle: Arc<Mutex<Handle>>,
    #[allow(clippy::type_complexity)]
    pub rx: Arc<Mutex<Receiver<(String, Box<dyn MessageDyn>)>>>,
}

impl SandboxService {
    pub fn new(rx: Receiver<(String, Box<dyn MessageDyn>)>) -> Result<Self> {
        let handle = Handle::new()?;
        Ok(Self {
            namespace: NAMESPACE.to_string(),
            handle: Arc::new(Mutex::new(handle)),
            rx: Arc::new(Mutex::new(rx)),
        })
    }

    pub(crate) async fn handle_localhost(&self) -> Result<()> {
        self.handle.lock().await.enable_lo().await
    }
}

#[async_trait]
impl api::sandbox_ttrpc::SandboxService for SandboxService {
    async fn update_interfaces(
        &self,
        _ctx: &TtrpcContext,
        req: UpdateInterfacesRequest,
    ) -> TtrpcResult<Empty> {
        self.handle
            .lock()
            .await
            .update_interfaces(req.interfaces)
            .await?;
        Ok(Empty::new())
    }

    async fn update_routes(
        &self,
        _ctx: &TtrpcContext,
        req: UpdateRoutesRequest,
    ) -> TtrpcResult<Empty> {
        self.handle.lock().await.update_routes(req.routes).await?;
        Ok(Empty::new())
    }

    async fn setup_sandbox(
        &self,
        _ctx: &TtrpcContext,
        req: SetupSandboxRequest,
    ) -> TtrpcResult<Empty> {
        match req.config.type_url.as_str() {
            "PodSandboxConfig" => {
                let config =
                    serde_json::from_slice::<PodSandboxConfig>(req.config.value.as_slice())
                        .map_err(|e| {
                            ttrpc::Error::Others(format!("convert PodSandboxConfig failed: {}", e))
                        })?;
                setup_sandbox(&config).await?;
            }
            _ => {
                return Err(ttrpc::Error::RpcStatus(ttrpc::get_status(
                    ::ttrpc::Code::NOT_FOUND,
                    format!(
                        "SetUpSandbox/config/{} is not supported",
                        &req.config.type_url
                    ),
                )));
            }
        }

        // Set interfaces
        self.handle
            .lock()
            .await
            .update_interfaces(req.interfaces)
            .await?;

        // Set Routes
        self.handle.lock().await.update_routes(req.routes).await?;

        Ok(Empty::new())
    }

    async fn check(&self, _ctx: &TtrpcContext, _req: CheckRequest) -> TtrpcResult<Empty> {
        Ok(Empty::new())
    }

    async fn exec_vm_process(
        &self,
        _ctx: &TtrpcContext,
        req: ExecVMProcessRequest,
    ) -> TtrpcResult<ExecVMProcessResponse> {
        let out = do_execute_cmd(&req.command, req.stdin.as_slice()).await?;

        let mut resp = ExecVMProcessResponse::new();
        resp.out = out;
        Ok(resp)
    }

    async fn sync_clock(
        &self,
        _ctx: &TtrpcContext,
        req: SyncClockPacket,
    ) -> TtrpcResult<SyncClockPacket> {
        let mut resp = req.clone();
        let clock_id = ClockId::from_raw(nix::libc::CLOCK_REALTIME);
        match req.Delta {
            0 => {
                resp.ClientArriveTime = clock_gettime(clock_id)
                    .map_err(Error::Nix)?
                    .num_nanoseconds();
                resp.ServerSendTime = clock_gettime(clock_id)
                    .map_err(Error::Nix)?
                    .num_nanoseconds();
            }
            _ => {
                let mut clock_spce = clock_gettime(clock_id).map_err(Error::Nix)?;
                clock_spce = clock_spce.add(TimeSpec::from_duration(Duration::from_nanos(
                    req.Delta as u64,
                )));
                clock_settime(clock_id, clock_spce).map_err(Error::Nix)?;
            }
        }
        Ok(resp)
    }

    async fn get_events(&self, _ctx: &TtrpcContext, _: Empty) -> TtrpcResult<Envelope> {
        while let Some((topic, event)) = self.rx.lock().await.recv().await {
            debug!("received event {:?}", event);
            // Only OOM Event is supported.
            // TODO: Support all topic
            if topic != TASK_OOM_EVENT_TOPIC {
                continue;
            }

            let mut resp = Envelope::new();
            resp.set_timestamp(SystemTime::now().into());
            resp.set_namespace(self.namespace.to_string());
            resp.set_topic(topic);
            resp.set_event(convert_to_any(event).unwrap());
            return Ok(resp);
        }

        Err(ttrpc::Error::Others("internal".to_string()))
    }
}

async fn do_execute_cmd(cmd_args: &str, stdin: &[u8]) -> Result<String> {
    let mut cmd = tokio::process::Command::new("/bin/bash");
    cmd.arg("-c");
    cmd.arg(cmd_args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    if !stdin.is_empty() {
        cmd.stdin(Stdio::piped());
    }

    let mut child = cmd
        .spawn()
        .map_err(io_error!(e, "spawn exec vm process failed:"))?;
    if !stdin.is_empty() {
        let cmd_in = child.stdin.as_mut().ok_or(other!("no stdin for command"))?;
        cmd_in
            .write_all(stdin)
            .await
            .map_err(io_error!(e, "failed to write vm process stdin:"))?;
    }

    let output = child
        .wait_with_output()
        .await
        .map_err(io_error!(e, "failed to combined process output:"))?;
    if output.status.success() {
        let raw_output =
            String::from_utf8(output.stdout).map_err(other_error!(e, "failed to convert"))?;
        Ok(raw_output)
    } else {
        let err_msg =
            String::from_utf8(output.stderr).map_err(other_error!(e, "failed to convert"))?;
        Err(other!(
            "cmd {} failed with status {:?} and error message {}",
            cmd_args,
            output.status,
            err_msg
        ))
    }
}
