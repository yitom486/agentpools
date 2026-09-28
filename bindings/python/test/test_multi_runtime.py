import os
import unittest
from pathlib import Path

from agentpools import AgentPool


ROOT = Path(__file__).resolve().parents[3]
MOCK = ROOT / "target" / "debug" / ("mock_runtime.exe" if os.name == "nt" else "mock_runtime")


@unittest.skipUnless(MOCK.exists(), "build the agentpools-runtime mock binary before integration tests")
class MultiRuntimeTests(unittest.IsolatedAsyncioTestCase):
    async def test_api_v2_mixes_codex_and_pi_workers(self):
        async with AgentPool({
            "apiVersion": 2,
            "agents": [
                {"runtime": "codexAppServer", "program": str(MOCK), "args": ["codex"], "cwd": str(ROOT)},
                {"runtime": "piRpc", "program": str(MOCK), "args": ["pi"], "cwd": str(ROOT)},
            ],
        }) as pool:
            for index, prefix in ((0, "codex"), (1, "pi")):
                async with await pool.acquire(index) as lease:
                    self.assertEqual((await lease.ask("hello"))["text"], f"{prefix}:hello:1")
                    self.assertEqual((await lease.ask("again"))["text"], f"{prefix}:again:2")

    async def test_api_v2_shared_process_and_ephemeral_codex(self):
        async with AgentPool({
            "apiVersion": 2,
            "sharedProcess": True,
            "agents": [
                {"runtime": "codexAppServer", "program": str(MOCK), "args": ["codex"], "cwd": str(ROOT), "ephemeral": True},
                {"runtime": "codexAppServer", "program": str(MOCK), "args": ["codex"], "cwd": str(ROOT), "ephemeral": True},
            ],
        }) as pool:
            l1 = await pool.acquire(0)
            l2 = await pool.acquire(1)
            try:
                self.assertEqual((await l1.ask("task-1"))["text"], "codex:task-1:1")
                self.assertEqual((await l2.ask("task-2"))["text"], "codex:task-2:2")
            finally:
                await l1.finish()
                await l2.finish()
