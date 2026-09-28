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
