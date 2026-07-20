# Course-snatching

Windows 桌面端教务选课助手：程序内登录、按精确课程序号监控余量，支持自动抢课或仅监控提醒。

仓库：<https://github.com/BaoZaoLG/Course-snatching>

> 请合理设置请求间隔，仅使用本人账号，并遵守学校相关规定。自动选课存在失败与限流风险，结果以教务系统为准。

## 功能概览

- **抢课 / 仅监控** 两种模式（仅监控时按余量变化提醒，不自动提交）
- 监控列表优先级调整（↑↓）；可选「优先检查有余量」
- **精确定时开抢** + **开抢冲刺**（开课后短窗口内高频轮询、去掉正抖动）
- 结果通知与提示音独立开关；结果摘要可导出
- 限流自适应、`Retry-After`、会话保活、停止中状态保护
- 选课成功二次确认，降低误报
- 密码不落盘；配置原子保存；深色模式

## 运行

### 发布包

从 [Releases](https://github.com/BaoZaoLG/Course-snatching/releases) 下载 zip，运行 `Course-snatching.exe`。

### 源码构建

需要 **Rust 1.97.1**（见 `rust-toolchain.toml`）：

```powershell
cargo run --release
```

产物路径：

```text
target\release\Course-snatching.exe
```

## 使用流程

1. 填写教务 `base_url`（HTTPS）、学号与密码。
2. 选课轮次通常留空自动探测；失败时再手动填写数字 ID。
3. 登录并刷新课程列表。
4. 将目标课加入监控（完整课程序号）；用 ↑↓ 调整优先级。
5. 按需开启「仅监控」或保持自动抢课。
6. 设置间隔：开课冲刺建议 `0.05`～`0.15` 秒；日常监控可更大。
7. 需要定时：开启定时开抢并设准时间，保持已登录状态等待触发。

## 配置与隐私

配置文件：

```text
%APPDATA%\Course-snatching\config.toml
```

- 保存账号、监控目标与界面选项
- **不保存密码**
- 调试页默认关闭；`runtime/debug` 仅排障时使用

清理本地调试残留与误放 exe：

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

跳过测试：

```powershell
$env:COURSE_SNATCHING_SKIP_TESTS = "1"
.\scripts\package.ps1
```

可选签名：设置 `COURSE_SNATCHING_SIGN_THUMBPRINT` 为证书指纹。

## 发布

推送版本 tag 触发 GitHub Actions 构建并创建 Release：

```powershell
git tag v0.10.1
git push origin v0.10.1
```

## 目录结构

```text
src/
  app/       界面与主题
  worker/    后台任务（登录 / 抢课 / 保活 / 定时）
  eams/      教务协议客户端与解析
  config.rs  配置
  notify.rs  通知与提示音
scripts/     打包、审计、隐私清理
assets/      图标
```

## 许可证

见 [LICENSE](LICENSE)。
