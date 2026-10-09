# AGENTS.md — wong-tools

桌面工具箱应用。Rust + `gpui-kit = "0.7.1"`（通过 `gpui_kit::*` 重导出
GPUI）。UI 风格沿用 SpurGit（自绘标题栏、侧边栏外壳、语义化主题 token）。

## 命令（Windows，PowerShell 5.1）

- 验证：`cargo check`（首次完整编译约 2 分钟，增量只需几秒）。
- 运行：`cargo run`。发布：`cargo build --release --locked`。
- 查看 cargo 输出用 `... 2>&1 | Select-Object -Last 40`。绝不要用
  `Select-Object -First N`——它会中途杀掉 cargo 进程。
- 提示找不到 `cargo`：当前终端环境变量太旧。cargo 在
  `%USERPROFILE%\.cargo\bin`（用户 PATH）。彻底重启编辑器/终端，
  或在当前 shell 刷新：
  `$env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')`
- 链接器报错 `link.exe not found`：缺少 VS 2022 生成工具的 C++
  工作负载（MSVC + Windows SDK），装好后重试。

## 架构

- `src/main.rs`——只做启动：字体 → `gpui_kit::init` → `theme::init`
  → `open_window`。不写任何 UI 代码。
- `src/shell.rs`——`WsShell`：38px 自绘标题栏（品牌 + 拖拽区 + 经
  `WindowControlArea` 接管的原生标题栏按钮）+ 216px 侧边导航 +
  内容区。工具目前只是 `TOOLS` 里的占位项，以后每个真工具配独立视图。
- `src/theme.rs`——用 `include_str!` 解析 `themes/wong.json`
  （`ThemeSet`），再经
  `Theme::global_mut(cx).apply_config(&Rc::new(config))` 应用 `Wong Dark`。
- `themes/wong.json`——6 套主题（默认 Wong Dark，外加 Ocean / Forest /
  Ember / Rose / Light），token 沿用 Spur。保留 `url` 里的 Zeron 署名。
- `assets/fonts/`——内置 Geist + Geist Mono（OFL 协议），首帧前经
  `include_bytes!` 注册。`Geist-OFL.txt` 和 `THIRD_PARTY_NOTICES.md`
  必须留在旁边。

## gpui-kit 0.7 注意事项（已对照 registry 源码验证）

- `open_window` 会自己把内容视图包进 `base::Root`——构建闭包只返回
  内容 entity，绝不要再包一层 `Root`。
- 完整 Lucide 图标集是 `gpui_kit::assets::IconName`，配合
  `.with_assets(assets::AllAssets)` 使用（默认 `Assets` 只是子集）。
- `window_control_area()` 是 `Div` 的固有方法，无需引入 trait。
- 动态元素 id：`ElementId::named_usize("nav", ix)`（`&'static str`、
  `String`、`usize` 也可用）。
- 样式辅助都挂在主题字段上（`title_bar`、`sidebar`、`border`…）；
  `link` 是真正可读的实色点缀色，`accent` 只是低透明度的 wash。
- `Cargo.toml` 的 `[profile.dev.package]` opt-level 覆盖是刻意的
  （只优化框架依赖，应用自身保持可调试）。`Cargo.lock` 保持提交。
  永远不要提交 `target/`。

## 开发规范（注释与测试）

- 注释写“为什么”，不复述代码在“做什么”。自解释的代码不加废话注释。
- 注释一律使用中文（`//!`、`///`、`//` 都是）。代码标识符保持英文；
  用户可见文案中文优先。
- 每个 `.rs` 文件首行必须有 `//!` 模块文档，说明职责与归属。
- 所有 `pub` 项必须有 `///` 文档注释；魔法数字必须命名为常量并注明
  来源（如 `CAPTION_W = 46.0` 是 Windows 原生标题栏按钮宽度）。
- 涉及平台差异（Windows / macOS）、gpui-kit 版本行为、或从 SpurGit
  照搬的模式，必须在注释里写明原因，方便以后升级 gpui-kit 时复查。
- 纯逻辑必须配单元测试（普通 `#[test]` 即可）：主题 JSON 解析、静态
  配置表（`TOOLS` 非空、重名检查）、窗口参数（透明标题栏、最小尺寸）。
- 需要跑真实窗口的 UI 交互测试依赖 `test-support` feature，本仓库暂不
  引入——不要为测 UI 而加它，逻辑层先行可测。
- 测试模块里禁止 `use super::*`：它会把 gpui 自带的 `#[test]` 宏引进
  来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
- 提交前必须 `cargo check` 零警告 + `cargo test` 全过。测试挂了先修
  测试或代码，绝不删断言蒙混过关。
