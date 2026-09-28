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
            lease = await pool.acquire()
            self.assertEqual(await lease.ask("first task"),
                {"text": "mock:first task", "stopReason": "end_turn"})
            self.assertEqual(await lease.ask("second task"),
                {"text": "mock:second task", "stopReason": "end_turn"})
            await lease.finish()
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
            lease = await pool.acquire()
            with self.assertRaisesRegex(RuntimeError, "mock failure"):
                await lease.ask("initial")
            response = await lease.ask("Attempt 1 failed: mock failure. Please retry.")
            self.assertIn("mock failure", response["text"])
            await lease.finish()
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

    async def test_external_validation_keeps_session_leased(self):
        directory, log_path, options = fixture()
        pool = AgentPool(options)
        lease = None
        try:
            lease = await pool.acquire()
            self.assertEqual(lease.agent_index, 0)
            self.assertEqual(await lease.ask("draft"), {"text": "mock:draft", "stopReason": "end_turn"})
            queued = asyncio.create_task(pool.acquire())
            await asyncio.sleep(0.05)
            # The caller validates the draft here, outside any agent call.
            self.assertEqual(pool.status(), {"queued": 1, "active": 1, "closed": False})
            self.assertEqual(
                await lease.ask("correction"),
                {"text": "mock:correction", "stopReason": "end_turn"},
            )
            self.assertEqual(pool.status()["queued"], 1)
            await lease.finish()
            lease = None
            next_lease = await queued
            self.assertEqual(
                await next_lease.ask("next task"),
                {"text": "mock:next task", "stopReason": "end_turn"},
            )
            await next_lease.finish()
            await pool.close(drain=True)

            messages = [json.loads(line) for line in log_path.read_text().splitlines()]
            self.assertEqual(sum(m.get("method") == "session/new" for m in messages), 1)
            prompts = [
                m["params"]["prompt"][0]["text"]
                for m in messages
                if m.get("method") == "session/prompt"
            ]
            self.assertEqual(prompts, ["draft", "correction", "next task"])
        finally:
            if lease is not None:
                await lease.finish()
            await pool.close(drain=False)
            shutil.rmtree(directory, ignore_errors=True)

if __name__ == "__main__":
    unittest.main()
