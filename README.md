# 选课助手（course-monitor）

面向郑州西亚斯学院教务系统的 Windows 桌面工具：程序内登录、读取可选课程、按**精确课程序号**监控余量，并在确认有余量时提交选课。

> 自动请求和自动选课可能受到学校规则与服务器限流约束。请合理设置间隔，仅用于本人账号，并自行确认使用符合学校规定。

## 主要特性

- 监控卡片展示课程名/教师/余量、检查次数与上次检查时间；成功沉底、可清理失败/成功
- 开始抢课前确认清单与开抢自检；结束后结果摘要
- 定时开抢（高级设置指定时间）
- 抢课结果通知、提示音；限流自适应间隔与会话保活
- 课程排序/筛选记忆、日志筛选导出、配置导入导出
- 首次引导、危险操作确认、界面缩放与深色模式
- 密码不写入磁盘，登录完成后从界面内存中清理
- 默认自动探测选课轮次，也支持手动填写数字 `profile_id`
- 自动选课只接受精确课程序号；多条匹配会停止该目标，不会随机选择
- 登录失效、HTTP 异常、连续网络失败会自动识别并停止/退避
- 轮询加入随机抖动，连续失败采用指数退避
- 运行期间锁定地址、轮次、间隔和监控目标，避免界面与后台状态不一致
- 配置使用临时文件和备份替换，损坏时保留备份
- 原始调试页面默认关闭，避免无意保存个人信息

## 运行

直接运行发布目录中的 `course-monitor.exe`。

从源码运行：

```powershell
cargo run --release
```

## 使用流程

1. 输入学号和密码。
2. 通常将“选课轮次”留空，让程序自动探测；只有自动探测失败时才手动填写数字 ID。
3. 登录并刷新课程。
4. 从课程列表加入监控目标，或手动输入完整课程序号。
5. 确认课程名称、教师、序号和监控间隔后开始。
6. 运行期间如需修改目标或设置，请先停止。

## 配置与隐私

配置文件默认位于：

```text
%APPDATA%\CourseMonitor\config.toml
```

旧版本 EXE 同目录下的 `config.toml` 会自动迁移。配置保存账号、轮询设置和监控目标，但不保存密码。

调试输出位于 EXE 同目录的 `runtime/debug/`。调试页面可能含有教务页面中的个人信息，仅排障时短暂开启，使用后请及时删除，不要公开上传。

## 开发与验证

要求 Rust 1.97.1。提交前应全部通过：

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --release
cargo package --list --allow-dirty
```

项目包含 Windows CI，会执行格式、Clippy、测试和 Release 构建。

## 代码结构

- `src/app.rs`：egui 界面与交互
- `src/config.rs`：配置迁移、校验和安全保存
- `src/eams.rs`：登录、HTTP 请求、轮次/课程解析和选课提交
- `src/worker.rs`：后台 runtime、状态机、退避、停止和结果统计

## 教务接口说明

- 登录：`loginExt.action` + 动态 salt SHA1
- 轮次入口：`stdElectCourse!defaultPage.action`
- 课程数据：`stdElectCourse!data.action`
- 已选人数：`stdElectCourse!queryStdCount.action`
- 提交选课：`stdElectCourse!batchOperator.action`

接口属于教务系统内部实现，可能随系统升级变化。解析失败时应先使用脱敏后的测试夹具复现，再更新解析器。
