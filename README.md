# 选课助手（course-monitor）

面向郑州西亚斯学院教务系统的 **Windows 桌面**工具：程序内登录、按**精确课程序号**监控余量，并支持自动抢课或仅监控提醒。

> 自动请求和自动选课可能受学校规则与服务器限流约束。请合理设置间隔，仅用于本人账号，并自行确认使用符合学校规定。

仓库：<https://github.com/BaoZaoLG/Course-snatching>

## 主要特性

- **抢课 / 仅监控**两种模式：仅监控时发现余量变化才提醒，不自动提交
- 监控列表 **↑↓ 优先级**；可「优先检查有余量」
- 监控卡片：课程名/教师/余量、检查次数、上次检查时间；成功沉底
- 开始前确认清单与自检；结束后结果摘要（可导出）
- 定时开抢（年月日时分秒）
- 结果通知与提示音可独立开关
- 限流自适应、`Retry-After`、会话保活、停止中状态保护
- 选课成功后二次确认（人数/列表变化），降低误报
- 密码不落盘；账号日志脱敏；配置原子保存
- 深色模式、界面缩放、日志筛选导出、配置导入导出

## 运行

### 发布包

从 [Releases](https://github.com/BaoZaoLG/Course-snatching/releases) 下载 zip，运行其中的 `course-monitor.exe`。

### 源码

要求 **Rust 1.97.1**（见 `rust-toolchain.toml`）：

```powershell
cargo run --release
```

## 使用流程

1. 输入学号和密码。
2. 选课轮次通常留空自动探测；失败时再手动填数字 ID。
3. 登录并刷新课程。
4. 从列表加入监控，或手动输入完整课程序号；用 ↑↓ 调整优先级。
5. 需要自动提交则保持「仅监控」关闭；只想盯余量则勾选「仅监控」。
6. 确认目标与间隔后开始；修改设置前请先停止。

## 配置与隐私

配置文件默认：

```text
%APPDATA%\CourseMonitor\config.toml
```

旧版 EXE 同目录配置会自动迁移。配置保存账号与监控目标，**不保存密码**。

调试输出在 `runtime/debug/`（默认关闭）。调试页可能含个人信息，排障后请删除，勿上传。

清理脚本：

```powershell
.\scripts\clean-privacy.ps1
```

## 开发与验证

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
.\scripts\audit.ps1
```

本地打包（默认先跑测试，生成 SHA256 / BUILD_INFO / SBOM）：

```powershell
.\scripts\package.ps1
```

跳过测试打包：

```powershell
$env:COURSE_MONITOR_SKIP_TESTS = "1"
.\scripts\package.ps1
```

可选 Authenticode：设置环境变量 `COURSE_MONITOR_SIGN_THUMBPRINT` 为证书指纹。

## 发布

推送版本 tag 会触发 GitHub Actions 构建并创建 Release：

```powershell
git tag v0.10.1
git push origin v0.10.1
```

## 架构概览

```text
src/
  app/       UI（egui）与主题
  worker/    后台登录 / 抢课循环 / 保活
  eams/      教务 HTTP 客户端与解析
  config.rs  配置读写
  notify.rs  应用内通知与提示音
```

## 许可证

见 [LICENSE](LICENSE)。
