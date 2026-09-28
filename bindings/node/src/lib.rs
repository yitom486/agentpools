use std::sync::{Arc, Mutex};

use agentpools::{AgentPool, CancellationToken, LeaseHandle, SessionLease, ShutdownMode};
use agentpools_runtime::{RuntimeBackend, RuntimePrompt, build_pool};
use napi::bindgen_prelude::*;
use napi_derive::napi;
use serde_json::json;

type Pool = AgentPool<RuntimeBackend>;
type RuntimeLeaseHandle = LeaseHandle<RuntimeBackend>;
type RuntimeLease = SessionLease<RuntimeBackend>;

#[napi]
pub struct NativeAgentPool {
    pool: Mutex<Option<Pool>>,
}

#[napi]
impl NativeAgentPool {
    #[napi]
    pub fn request_lease(&self, agent_index: Option<u32>) -> Result<NativeSessionLease> {
        let pool = self.pool.lock().unwrap_or_else(|error| error.into_inner());
        let pool = pool
            .as_ref()
            .ok_or_else(|| Error::from_reason("agent pool is closed"))?;
        let handle = if let Some(index) = agent_index {
            pool.request_lease_to(index as usize)
        } else {
            pool.request_lease()
        }
        .map_err(|error| Error::from_reason(error.to_string()))?;
        Ok(NativeSessionLease::new(handle))
    }

    #[napi]
    pub fn status_json(&self) -> Result<String> {
        let pool = self.pool.lock().unwrap_or_else(|error| error.into_inner());
        let Some(pool) = pool.as_ref() else {
            return Ok(json!({ "queued": 0, "active": 0, "closed": true }).to_string());
        };
        let status = pool.status();
        Ok(json!({
            "queued": status.queued,
            "active": status.active,
            "closed": status.closed
        })
        .to_string())
    }

    #[napi]
    pub fn close(&self, drain: Option<bool>) -> Result<AsyncTask<ClosePoolTask>> {
        let pool = self
            .pool
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let mode = if drain.unwrap_or(true) {
            ShutdownMode::Drain
        } else {
            ShutdownMode::CancelPending
        };
        Ok(AsyncTask::new(ClosePoolTask { pool, mode }))
    }
}

#[napi]
pub fn create_pool(config_json: String) -> Result<NativeAgentPool> {
    let pool = build_pool(&config_json)
        .map_err(|error| Error::from_reason(format!("invalid pool configuration: {error}")))?;
    Ok(NativeAgentPool {
        pool: Mutex::new(Some(pool)),
    })
}

impl Drop for NativeAgentPool {
    fn drop(&mut self) {
        let pool = self
            .pool
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(mut pool) = pool {
            let _ = std::thread::Builder::new()
                .name("agentpools-node-finalize".into())
                .spawn(move || drop(pool.shutdown(ShutdownMode::CancelPending)));
        }
    }
}

#[napi]
pub struct NativeSessionLease {
    cancellation: CancellationToken,
    request: Mutex<Option<RuntimeLeaseHandle>>,
    lease: Arc<Mutex<Option<RuntimeLease>>>,
}

impl NativeSessionLease {
    fn new(handle: RuntimeLeaseHandle) -> Self {
        Self {
            cancellation: handle.cancellation_token(),
            request: Mutex::new(Some(handle)),
            lease: Arc::new(Mutex::new(None)),
        }
    }
}

#[napi]
impl NativeSessionLease {
    #[napi]
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    #[napi]
    pub fn ready(&self) -> Result<AsyncTask<WaitLeaseTask>> {
        let handle = self
            .request
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .ok_or_else(|| Error::from_reason("lease request can only be awaited once"))?;
        Ok(AsyncTask::new(WaitLeaseTask {
            handle: Some(handle),
            lease: Arc::clone(&self.lease),
        }))
    }

    #[napi]
    pub fn ask(&self, prompt_json: String) -> Result<AsyncTask<AskLeaseTask>> {
        let prompt = parse_prompt(&prompt_json)?;
        let lease = self
            .lease
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .ok_or_else(|| Error::from_reason("session lease is not ready, busy, or finished"))?;
        Ok(AsyncTask::new(AskLeaseTask {
            lease: Some(lease),
            storage: Arc::clone(&self.lease),
            prompt: Some(prompt),
        }))
    }

    #[napi]
    pub fn finish(&self) -> Result<AsyncTask<FinishLeaseTask>> {
        let lease = self
            .lease
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .ok_or_else(|| Error::from_reason("session lease is not ready, busy, or finished"))?;
        Ok(AsyncTask::new(FinishLeaseTask { lease: Some(lease) }))
    }
}

pub struct WaitLeaseTask {
    handle: Option<RuntimeLeaseHandle>,
    lease: Arc<Mutex<Option<RuntimeLease>>>,
}

impl Task for WaitLeaseTask {
    type Output = u32;
    type JsValue = u32;

    fn compute(&mut self) -> Result<Self::Output> {
        let handle = self
            .handle
            .take()
            .ok_or_else(|| Error::from_reason("lease request was already consumed"))?;
        let lease = handle
            .wait()
            .map_err(|error| Error::from_reason(error.to_string()))?;
        let agent_index = lease.agent_index() as u32;
        *self.lease.lock().unwrap_or_else(|error| error.into_inner()) = Some(lease);
        Ok(agent_index)
    }

    fn resolve(&mut self, _: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

pub struct AskLeaseTask {
    lease: Option<RuntimeLease>,
    storage: Arc<Mutex<Option<RuntimeLease>>>,
    prompt: Option<RuntimePrompt>,
}

impl Task for AskLeaseTask {
    type Output = String;
    type JsValue = String;

    fn compute(&mut self) -> Result<Self::Output> {
        let mut lease = self
            .lease
            .take()
            .ok_or_else(|| Error::from_reason("session lease was already consumed"))?;
        let prompt = self
            .prompt
            .take()
            .ok_or_else(|| Error::from_reason("prompt was already consumed"))?;
        let result = lease.ask(prompt);
        *self
            .storage
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(lease);
        let response = result.map_err(|error| Error::from_reason(error.to_string()))?;
        serde_json::to_string(&response)
            .map_err(|error| Error::from_reason(format!("cannot encode task response: {error}")))
    }

    fn resolve(&mut self, _: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

pub struct FinishLeaseTask {
    lease: Option<RuntimeLease>,
}

impl Task for FinishLeaseTask {
    type Output = bool;
    type JsValue = bool;

    fn compute(&mut self) -> Result<Self::Output> {
        self.lease
            .take()
            .ok_or_else(|| Error::from_reason("session lease was already consumed"))?
            .finish()
            .map_err(|error| Error::from_reason(error.to_string()))?;
        Ok(true)
    }

    fn resolve(&mut self, _: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

pub struct ClosePoolTask {
    pool: Option<Pool>,
    mode: ShutdownMode,
}

impl Task for ClosePoolTask {
    type Output = String;
    type JsValue = String;

    fn compute(&mut self) -> Result<Self::Output> {
        let Some(mut pool) = self.pool.take() else {
            return Ok(json!({
                "closed": true,
                "closeErrors": [],
                "panickedWorkers": 0
            })
            .to_string());
        };
        let report = pool.shutdown(self.mode);
        Ok(json!({
            "closed": true,
            "closeErrors": report.close_errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "panickedWorkers": report.panicked_workers
        })
        .to_string())
    }

    fn resolve(&mut self, _: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

fn parse_prompt(prompt_json: &str) -> Result<RuntimePrompt> {
    serde_json::from_str(prompt_json)
        .map_err(|error| Error::from_reason(format!("invalid agent prompt: {error}")))
}
