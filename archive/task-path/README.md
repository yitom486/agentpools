# 已归档的直接提交路径

这里保存移除前的 src/task.rs、src/pool.rs、src/error.rs 和 tests/pool.rs 快照，供对照学习，不参与当前编译。历史实现中的 TaskHandle::wait 在响应到达后便允许 worker 接新任务，无法覆盖调用方随后的业务校验。

当前 API 以租用为工作单位：acquire 或 request_lease 获得 SessionLease；多轮 ask、校验、反馈期间始终占有 worker；finish 或丢弃租用才释放。参见 [../../src/README.md](../../src/README.md)。
