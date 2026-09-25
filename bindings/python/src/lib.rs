use std::num::NonZeroUsize;
use std::sync::Mutex;

use agentpools::{
    AgentPool, CancellationToken, LeaseHandle, SessionLease, ShutdownMode, TaskHandle,
};
use agentpools_acp::{AcpBackend, AcpError, AcpPoolOptions, AcpPrompt, AcpResponse};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use serde_json::json;

type Pool = AgentPool<AcpBackend>;
type Handle = TaskHandle<AcpResponse, AcpError>;
type AcpLeaseHandle = LeaseHandle<AcpBackend>;
type AcpLease = SessionLease<AcpBackend>;

#[pyclass(name = "NativeAgentPool")]
struct PyAgentPool {
    pool: Mutex<Option<Pool>>,
}

#[pymethods]
impl PyAgentPool {
    #[new]
    fn new(config_json: &str) -> PyResult<Self> {
        let options = serde_json::from_str::<AcpPoolOptions>(config_json).map_err(|error| {
            PyValueError::new_err(format!("invalid pool configuration: {error}"))
        })?;
        let pool = options
            .build()
            .map_err(|error| PyValueError::new_err(error.to_string()))?;
        Ok(Self {
            pool: Mutex::new(Some(pool)),
        })
    }

    fn submit(&self, prompt_json: &str, agent_index: Option<u32>) -> PyResult<PyTask> {
        let prompt = parse_prompt(prompt_json)?;
        let pool = self.pool.lock().unwrap_or_else(|error| error.into_inner());
        let pool = pool
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("agent pool is closed"))?;
        let handle = if let Some(index) = agent_index {
            pool.submit_to(index as usize, prompt)
        } else {
            pool.submit(prompt)
        }
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
        Ok(PyTask::new(handle))
    }

    fn request_lease(&self, agent_index: Option<u32>) -> PyResult<PyLeaseRequest> {
        let pool = self.pool.lock().unwrap_or_else(|error| error.into_inner());
        let pool = pool
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("agent pool is closed"))?;
        let handle = if let Some(index) = agent_index {
            pool.request_lease_to(index as usize)
        } else {
            pool.request_lease()
        }
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
        Ok(PyLeaseRequest {
            cancellation: handle.cancellation_token(),
            handle: Mutex::new(Some(handle)),
        })
    }

    fn submit_retrying(
        &self,
        prompt_json: &str,
        max_attempts: u32,
        feedback_template: String,
        agent_index: Option<u32>,
    ) -> PyResult<PyTask> {
        let prompt = parse_prompt(prompt_json)?;
        let attempts = NonZeroUsize::new(max_attempts as usize)
            .ok_or_else(|| PyValueError::new_err("max_attempts must be greater than zero"))?;
        let retry_base = prompt.clone();
        let retry = move |error: &AcpError, attempt: usize| {
            let feedback = render_feedback(&feedback_template, error, attempt);
            Some(retry_base.with_retry_feedback(feedback))
        };

        let pool = self.pool.lock().unwrap_or_else(|error| error.into_inner());
        let pool = pool
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("agent pool is closed"))?;
        let handle = if let Some(index) = agent_index {
            pool.submit_retrying_to(index as usize, prompt, attempts, retry)
        } else {
            pool.submit_retrying(prompt, attempts, retry)
        }
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
        Ok(PyTask::new(handle))
    }

    fn status_json(&self) -> PyResult<String> {
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

    fn close(&self, py: Python<'_>, drain: bool) -> PyResult<String> {
        let pool = self
            .pool
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        Ok(py.detach(move || close_pool(pool, drain)))
    }
}

impl Drop for PyAgentPool {
    fn drop(&mut self) {
        let pool = self
            .pool
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(mut pool) = pool {
            let _ = std::thread::Builder::new()
                .name("agentpools-python-finalize".into())
                .spawn(move || drop(pool.shutdown(ShutdownMode::CancelPending)));
        }
    }
}

#[pyclass(name = "NativeTask")]
struct PyTask {
    id: String,
    cancellation: CancellationToken,
    handle: Mutex<Option<Handle>>,
}

impl PyTask {
    fn new(handle: Handle) -> Self {
        let id = handle.id().to_string();
        let cancellation = handle.cancellation_token();
        Self {
            id,
            cancellation,
            handle: Mutex::new(Some(handle)),
        }
    }
}

#[pyclass(name = "NativeLeaseRequest")]
struct PyLeaseRequest {
    cancellation: CancellationToken,
    handle: Mutex<Option<AcpLeaseHandle>>,
}

#[pymethods]
impl PyLeaseRequest {
    fn cancel(&self) {
        self.cancellation.cancel();
    }

    fn wait(&self, py: Python<'_>) -> PyResult<PySessionLease> {
        let handle = self
            .handle
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .ok_or_else(|| PyRuntimeError::new_err("lease request can only be awaited once"))?;
        let lease = py
            .detach(move || handle.wait().map_err(|error| error.to_string()))
            .map_err(PyRuntimeError::new_err)?;
        Ok(PySessionLease::new(lease))
    }
}

#[pyclass(name = "NativeSessionLease")]
struct PySessionLease {
    agent_index: usize,
    lease: Mutex<Option<AcpLease>>,
}

impl PySessionLease {
    fn new(lease: AcpLease) -> Self {
        Self {
            agent_index: lease.agent_index(),
            lease: Mutex::new(Some(lease)),
        }
    }
}

#[pymethods]
impl PySessionLease {
    #[getter]
    fn agent_index(&self) -> usize {
        self.agent_index
    }

    fn ask(&self, py: Python<'_>, prompt_json: &str) -> PyResult<String> {
        let prompt = parse_prompt(prompt_json)?;
        py.detach(move || {
            let mut guard = self.lease.lock().unwrap_or_else(|error| error.into_inner());
            let lease = guard
                .as_mut()
                .ok_or_else(|| "session lease is finished".to_string())?;
            let response = lease.ask(prompt).map_err(|error| error.to_string())?;
            serde_json::to_string(&response).map_err(|error| error.to_string())
        })
        .map_err(PyRuntimeError::new_err)
    }

    fn finish(&self, py: Python<'_>) -> PyResult<()> {
        let lease = self
            .lease
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(lease) = lease {
            py.detach(move || lease.finish().map_err(|error| error.to_string()))
                .map_err(PyRuntimeError::new_err)?;
        }
        Ok(())
    }
}

#[pymethods]
impl PyTask {
    #[getter]
    fn id(&self) -> String {
        self.id.clone()
    }

    fn cancel(&self) {
        self.cancellation.cancel();
    }

    fn result(&self, py: Python<'_>) -> PyResult<String> {
        let handle = self
            .handle
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .ok_or_else(|| PyRuntimeError::new_err("task result can only be awaited once"))?;
        py.detach(move || {
            let response = handle.wait().map_err(|error| error.to_string())?;
            serde_json::to_string(&response).map_err(|error| error.to_string())
        })
        .map_err(PyRuntimeError::new_err)
    }
}

fn close_pool(pool: Option<Pool>, drain: bool) -> String {
    let Some(mut pool) = pool else {
        return json!({
            "closed": true,
            "closeErrors": [],
            "panickedWorkers": 0
        })
        .to_string();
    };
    let mode = if drain {
        ShutdownMode::Drain
    } else {
        ShutdownMode::CancelPending
    };
    let report = pool.shutdown(mode);
    json!({
        "closed": true,
        "closeErrors": report.close_errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "panickedWorkers": report.panicked_workers
    })
    .to_string()
}

fn parse_prompt(prompt_json: &str) -> PyResult<AcpPrompt> {
    serde_json::from_str(prompt_json)
        .map_err(|error| PyValueError::new_err(format!("invalid ACP prompt: {error}")))
}

fn render_feedback(template: &str, error: &AcpError, attempt: usize) -> String {
    template
        .replace("{error}", &error.to_string())
        .replace("{attempt}", &attempt.to_string())
}

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyAgentPool>()?;
    module.add_class::<PyTask>()?;
    module.add_class::<PyLeaseRequest>()?;
    module.add_class::<PySessionLease>()?;
    Ok(())
}
