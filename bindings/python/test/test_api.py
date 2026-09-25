import asyncio
import json
import os
import shutil
import tempfile
import unittest
from pathlib import Path

from agentpools import AgentPool


ROOT = Path(__file__).resolve().parents[3]
MOCK_AGENT = ROOT / "target" / "debug" / (
    "agentpools-acp-mock-agent.exe" if os.name == "nt" else "agentpools-acp-mock-agent"
)


def fixture(scenario=""):
    directory = Path(tempfile.mkdtemp(prefix="agentpools-python-"))
    log_path = directory / "acp.jsonl"
    options = {
        "apiVersion": 1,
        "maxQueued": 8,
        "agents": [
            {
                "program": str(MOCK_AGENT),
                "cwd": str(ROOT),
                "env": {
                    "AGENTPOOLS_MOCK_LOG": str(log_path),
                    "AGENTPOOLS_MOCK_SCENARIO": scenario,
                },
                "mcpServers": [
                    {
                        "name": "caller-calculator",
                        "command": "calculator-mcp",
                        "args": ["--readonly"],
                    }
                ],
            }
        ],
    }
    return directory, log_path, options


@unittest.skipUnless(MOCK_AGENT.exists(), "build the ACP mock agent before integration tests")
class BindingIntegrationTests(unittest.IsolatedAsyncioTestCase):
    async def test_async_tasks_reuse_session_and_forward_mcp(self):
        directory, log_path, options = fixture()
        pool = AgentPool(options)
        try:
            first = pool.submit("first task")
            second = pool.submit("second task")
            self.assertIsInstance(first.id, str)
            self.assertEqual(
                await asyncio.gather(first.result(), second.result()),
                [
                    {"text": "mock:first task", "stopReason": "end_turn"},
                    {"text": "mock:second task", "stopReason": "end_turn"},
                ],
            )
            report = await pool.close(drain=True)
            self.assertEqual(report, {"closed": True, "closeErrors": [], "panickedWorkers": 0})
            self.assertEqual(pool.status(), {"queued": 0, "active": 0, "closed": True})

            messages = [json.loads(line) for line in log_path.read_text().splitlines()]
            methods = [message["method"] for message in messages if "method" in message]
            self.assertEqual(methods.count("initialize"), 1)
            self.assertEqual(methods.count("session/new"), 1)
            self.assertEqual(methods.count("session/prompt"), 2)
            self.assertEqual(methods.count("session/close"), 1)
            session_new = next(m for m in messages if m.get("method") == "session/new")
            self.assertEqual(session_new["params"]["mcpServers"], options["agents"][0]["mcpServers"])
        finally:
            await pool.close(drain=False)
            shutil.rmtree(directory, ignore_errors=True)

    async def test_explicit_retry_reuses_same_session(self):
        directory, log_path, options = fixture("recover-on-feedback")
        pool = AgentPool(options)
        try:
            task = pool.submit_retrying(
                "initial",
                max_attempts=2,
                feedback_template="Attempt {attempt} failed: {error}. Please retry.",
            )
            response = await task.result()
            self.assertIn("mock failure", response["text"])
            await pool.close(drain=True)

            messages = [json.loads(line) for line in log_path.read_text().splitlines()]
            methods = [message["method"] for message in messages if "method" in message]
            self.assertEqual(methods.count("initialize"), 1)
            self.assertEqual(methods.count("session/new"), 1)
            self.assertEqual(methods.count("session/prompt"), 2)
            self.assertEqual(methods.count("session/close"), 1)
        finally:
            await pool.close(drain=False)
            shutil.rmtree(directory, ignore_errors=True)

if __name__ == "__main__":
    unittest.main()
