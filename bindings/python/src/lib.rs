use std::num::NonZeroUsize;
use std::sync::Mutex;

use agentpools::{AgentPool, CancellationToken, ShutdownMode, TaskHandle};
use agentpools_acp::{AcpBackend, AcpError, AcpPoolOptions, AcpPrompt, AcpResponse};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use serde_json::json;

type Pool = AgentPool<AcpBackend>;
type Handle = TaskHandle<AcpResponse, AcpError>;

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
    Ok(())
}
