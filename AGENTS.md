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
  内容区。截图和代理是真工具（状态在 `WsShell.shot` / `WsShell.proxy`）；
  其余工具仍是 `TOOLS` 占位项，各配独立视图；设置页是真交互
  （语言 / 主题切换），选中态暂存 `WsShell`。
- `src/tools/`——一个工具一个文件夹，逻辑 + 视图都关在里面，跨工具
  只认 `crate::tools::<工具名>::` 路径（状态类型在 tool 根重导出，
  不直接钻 `view`）。加新工具：在此建文件夹 + `TOOLS` 登记 +
  文案两列 + `KEYS` 登记，视图另起 `view.rs`（`impl WsShell`，
  字段/方法走 `pub(crate)`）。
- `src/shell.rs`——只剩框架：`WsShell`（字段 `pub(crate)`）、`TOOLS`、
  标题栏/侧边栏/分发、`Drop` 兜底。工具视图和设置页视图不在这。
- `src/tools/screenshot/mod.rs`——纯逻辑，不碰 GPUI：xcap 截主显示器
  （GDI），arboard 写剪贴板图片，image 编 PNG / 缩略图 / 落盘。预览经
  `img(ImageSource::Image(...))` 显示，id 每次截取必须换新。
  `view.rs` 是捕获页（预览 + 复制 / 保存 / 重截）。
- `src/tools/screenshot/overlay.rs`——区域选择覆盖层（全屏 PopUp +
  冻结帧 + 底部工具条）：根 `relative()`，冻结帧 flow 铺满，变暗 +
  描边走 canvas `paint_quad`（不经过布局），`canvas` 在 paint 里挂
  `window.on_mouse_event`（窗口级监听，拖出界不丢事件，shotori 同款）；
  纯矩形数学在同目录 `select.rs`，覆盖层只做组装。确定后经
  `WeakEntity::update` 写回主窗口并关窗。工具条吃掉左键按下
  （`stop_propagation`），不会误开选区。工具条跟选区走（下方→上方→
  底部居中，见 `select::toolbar_rect`），只在松手定稿后显示，绝不钉死
  在屏幕边缘。工具条是图标按钮 + tooltip + 分隔符（shotori 原样），
  盒子尺寸和命中判定用同一组常量，见 `select::TOOLBAR_W/H`。
  没选区时工具条不显示（`in_toolbar` 同样放行，按下直接起拖）。
- `src/tools/proxy/mod.rs`——代理工具：Clash 订阅解析（节点只收 VLESS，
  分组照单收，规则收 DOMAIN / SUFFIX / KEYWORD / CIDR / MATCH，GEOIP 等
  跑不了的记 `Unsupported` 跳过）、按规则分流（`DIRECT` 裸连、`REJECT`
  静默断、组名按组选择落节点，`url-test` 组永远拿延迟最低的，第一条
  命中的规则赢；8.7 万条后缀走排序向量 + 二分，索引一次建好）。
  VLESS + WS + TLS 隧道（`127.0.0.1:10809` 混合入口，SOCKS5 /
  HTTP CONNECT 都接，VLESS 响应头必须吃掉不能转发，否则 HTTPS 全挂）、
  注册表 + `wininet` 刷新做系统代理开关（`ProxyOverride=<local>` 本地直连）。
  阻塞式线程模型，不引入新异步运行时；拉订阅扔后台线程，别卡界面。
  分流表是活的（`LiveRouting` = `Arc<RwLock<RoutingTable>>`）：界面改选择、
  拉订阅、测完延迟都整表替换，每个连接进来读一次，已建连的不受影响，
  不用重连更不用重启软件。
  连接遥测走 `ConnRegistry`（`Mutex` 换手）：中继按块记上下行，规则文本 +
  分组链登记，界面每秒读快照算速率；进程列暂空（Windows 按连接归因进程
  要 IP Helper 轮询，以后再做）。
- 冻结帧走 `RenderImage` 直显（RGBA→BGRA 自己换一次）：全分辨率 PNG
  编解码在 debug 下要几秒，覆盖层必须零编码打开。开窗尺寸用传进去的
  逻辑界，别读构造期的 `window.bounds()`（不可靠）。
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
- `.child(&str)` 只要 `'static`：借来的文案先转 `SharedString` 再进
  元素树；`Pixels` 取值用 `.as_f32()`（字段私有）。
- 跨窗口写回走 `WeakEntity::update`（闭包里是 `&mut Context`，有
  `notify`）；拿本窗口弱引用用 `cx.entity().downgrade()`。
- 样式辅助都挂在主题字段上（`title_bar`、`sidebar`、`border`…）；
  `link` 是真正可读的实色点缀色，`accent` 只是低透明度的 wash。
- `Cargo.toml` 的 `[profile.dev.package]` opt-level 覆盖是刻意的
  （只优化框架依赖，应用自身保持可调试）。`Cargo.lock` 保持提交。
  永远不要提交 `target/`。

## gpui-kit 组件一览（v0.7.1，详见 https://gpui-kit.com/component/）

- 引入路径一律 `gpui_kit::component::<模块>`（如
  `button::{Button, ButtonVariants}`），单个组件用法去看对应文档页
  `https://gpui-kit.com/component/<kebab>`（如 `dropdown_button`）。
- 基础：Accordion / Alert / Attachment / Avatar / Badge / Bubble /
  Button / Checkbox / Collapsible / DropdownButton / Icon / Image /
  Kbd / Label / Marker / Message / MessageScroller / Pagination /
  Progress / Questionnaire / Radio / Rating / Skeleton / Slider /
  Spinner / Stepper / Switch / Tag / TextView / Toggle / Tooltip。
- 表单：Input / Textarea / Editor / Select / Combobox / NumberInput /
  DatePicker / TimeField / OtpInput / Speech / ColorPicker / Form。
- 布局：DescriptionList / GroupBox / Root（开窗已自带，别再包）/
  Theme / Dialog / Notification / Popover / Resizable / Scrollable /
  Sheet / Sidebar / StatusBar / Toolbar。
- 高级：Calendar / Carousel / Command / Chart / List / Menu /
  Settings / DataTable / Dock / Tabs / Tree / VirtualList。
- 总览页未分组、但侧边栏有文档的：AlertDialog / Clipboard / Empty /
  FocusTrap（focus-trap）/ HoverCard / InputGroup（input-group）/
  Plot / Shimmer / Table / TitleBar——用前先看对应文档页。

## 开发规范（注释与测试）

- 注释写“为什么”，不复述代码在“做什么”。自解释的代码不加废话注释。
- 注释一律使用中文（`//!`、`///`、`//` 都是）。代码标识符保持英文；
  用户可见文案中文优先。
- 所有用户可见文案必须走 `crate::i18n::t("app.xxx")`，禁止在视图里硬编码。
  默认中文，`WONG_LANG=en` 切英文。
- 引用文案时在上面加中文注释，注明中文原文（如 `// “新建”`），
  不翻 yml 就知道界面写的是什么。`ToolId::name/hint` 内部按 key 查表，
  每个分支自带原文注释。
- 文案住在 `locales/ui.yml`（`app.*` 命名空间，官方 rust-i18n 流程，
  详见 https://gpui-kit.com/docs/i18n）。加语言：加 locale 列 +
  `Language` 分支 + `LANGUAGES` 登记；加文案：两列同时加，并在
  `i18n::tests::KEYS` 登记。组件内置文案应用侧查不到，缺 key 时
  `t()` 返回 key 本身。
- `extend!(gpui_component)` 在启动时只调一次，且必须在 `gpui_kit::init`
  之前。语言切换走 `component::set_locale`（经 `i18n::set_language`）。
- 改全局 locale 的测试必须拿 `i18n::lock_locale()` 串行，否则并行跑
  互相覆盖。
- 语言运行时可切（`set_language` + `cx.notify()` 即时重绘），窗口标题
  等窗口级文字需重启生效；主题用 `theme::apply` 即时切换并重绘所有窗口。
- 设置持久化走 libsql 本地库（`src/settings.rs`，键值表 + 自动规则表
  `proxy_rules_auto`）：Windows 在 `%APPDATA%\WongTools\settings.db`，
  macOS 在 `~/Library/Application Support/WongTools/settings.db`。
  窗口位置（`window_bounds`，含最大化旗）启动恢复、越界居中，
  侧边栏选中（`selected_tool`）随点随存；位置平时 render 节流存、
  退出钩子补尾巴。
  订阅的节点/分组走小 JSON，8 万条自动规则走独立表（`pos` 保序，先清
  后插一个事务；旧大 JSON 键只读一次迁移，迁完即删）。读写失败只记日志不崩；
  `theme::apply` / `i18n::set_language` 内部顺手持久化，调用方不用管。
- 测持久化必须隔离：直连用 `open(temp路径)`，走公共 API 的用
  `TempSettings` 把 `WONG_SETTINGS_DB` 指向临时文件（配合
  `lock_locale()` 串行）。绝不碰真实数据目录。
- 每个 `.rs` 文件首行必须有 `//!` 模块文档，说明职责与归属。
- 所有 `pub` 项必须有 `///` 文档注释；魔法数字必须命名为常量并注明
  来源（如 `CAPTION_W = 46.0` 是 Windows 原生标题栏按钮宽度）。
- 涉及平台差异（Windows / macOS）、gpui-kit 版本行为、或从 SpurGit
  照搬的模式，必须在注释里写明原因，方便以后升级 gpui-kit 时复查。
- 纯逻辑必须配单元测试（普通 `#[test]` 即可）：主题 JSON 解析、静态
  配置表（`TOOLS` 非空、重名检查）、窗口参数（透明标题栏、最小尺寸）。
- 截图捕获 / 剪贴板测试要桌面会话，无头环境会挂——这是环境要求，
  不是代码问题，不要为此放宽断言。
- 需要跑真实窗口的 UI 交互测试依赖 `test-support` feature，本仓库暂不
  引入——不要为测 UI 而加它，逻辑层先行可测。
- 测试模块里禁止 `use super::*`：它会把 gpui 自带的 `#[test]` 宏引进
  来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
- 提交前必须 `cargo check` 零警告 + `cargo test` 全过。测试挂了先修
  测试或代码，绝不删断言蒙混过关。
