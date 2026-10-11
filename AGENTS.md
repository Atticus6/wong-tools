# AGENTS.md — wong-tools

桌面工具箱应用。Rust + `gpui-kit = "0.7.1"`（通过 `gpui_kit::*` 重导出
GPUI）。UI 风格沿用 SpurGit（自绘标题栏、侧边栏外壳、语义化主题 token）。

## 命令（Windows，PowerShell 5.1）

- 验证：`cargo check`（首次完整编译约 2 分钟，增量只需几秒）。
- 运行：`cargo run`。发布：`cargo build --release --locked`。
- 编译缓存（必装）：`sccache`（`choco install sccache` / `brew install sccache`），
  当前 shell 生效：`$env:RUSTC_WRAPPER="sccache"`（Windows，写进用户环境变量
  一劳永逸）；mac/Linux 用 `export RUSTC_WRAPPER=sccache`。`RUSTC_WRAPPER`
  只走环境变量、故意不进 `.cargo/config.toml`——没装 sccache 的机器会直接断构建。
- 测试用 `cargo nextest run --locked`（`cargo install cargo-nextest --locked`
  装一次，并行跑比 `cargo test` 快；配置在 `.config/nextest.toml`，没装才回落
  `cargo test`）。AI 改代码时只跑相关子集（如 `cargo nextest run --locked proxy::`），
  提交前才跑全量。CI（`.github/workflows/test.yml`）在 Windows 跑全量不断言过滤，
  缓存和打包线同款（sccache + rust-cache）。
- 打包：`packaging\build.ps1`（release 构建 + 便携 ZIP + Inno 安装包，
  版本号取 `Cargo.toml`，产物落 `dist/` 不提交；要装 Inno Setup 6，
  打包姿势见 https://gpui-kit.com/docs/packaging/)。
  应用图标在 `assets/icons/`（`make-icons.py` 纯标准库重生成，主图 +
  `.icns` / `.ico` / hicolor 全提交，CI 只拷贝）：mac 进 `.app` 的
  `Resources`，win 随包安装供快捷方式指，linux 进 hicolor 树。
  推 `v*` tag 或手动点就跑 `.github/workflows/package.yml`（三平台矩阵：
  win 走 `build.ps1`，linux 出 tarball + `.deb`，mac 出 arm64/x64 双架构
  `.app` ZIP + 未签名 DMG；sccache + rust-cache 提速，包齐后汇总建一次 Release）。
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
  `WindowControlArea` 接管的原生标题栏按钮）+ 侧边导航（216px，
  标题右侧（紧靠标题）收起按钮（官方 `SidebarToggleButton` 没透出 tooltip，
  自建同款 ghost 图标按钮）一键收起成 52px 图标列）+ 内容区。工具共 6 个：截图 / 代理 / AI 转发 / 咖啡是真工具
  （状态在 `WsShell.shot` / `WsShell.proxy` / `WsShell.airelay` / `WsShell.coffee`），
  欢迎是占位首页，设置页是真交互（语言 / 主题用 Combobox 下拉，
  状态 entity 放 `WsShell`，选中走订阅事件应用；聚光面板也能改，
  `render` 里对账防过期；版式照官方 Settings：描边卡片 + 左文右控件），选中态和收起态暂存 `WsShell`（收起随手持久化，启动恢复）。
- `src/tools/`——一个工具一个文件夹，逻辑 + 视图都关在里面，跨工具
  只认 `crate::tools::<工具名>::` 路径（状态类型在 tool 根重导出，
  不直接钻 `view`）。加新工具：在此建文件夹 + `TOOLS` 登记 +
  文案两列 + `KEYS` 登记，视图另起 `view.rs`（`impl WsShell`，
  字段/方法走 `pub(crate)`）。
- `src/spotlight/`——聚光搜索（独立 PopUp 面板窗口，不是侧边栏工具，
  所以不进 `tools/`，也不是主窗口里的悬浮层）：`mod.rs`（命令表/过滤/
  快捷键解析/`HotkeyHandle`）+ `view.rs`（主窗侧 `SpotlightView`
  开关 + 面板窗 `PaletteView`）。多窗口做法见官方 multi-window 文档：
  每窗独立 `Root`，面板执行命令回主窗口（`WeakEntity<WsShell>`），
  跑完自关并把主窗还原到前台（平台级 `activate_window`，最小化也拉回，
  再做应用级 `activate` 兜底）；关窗策略是全关才退。面板窗 mac 红绿灯用
  越界位置藏掉（框架没隐藏开关，升级时复查）。面板 Esc/执行/开关/窗外
  点击都关：输入框 `on_focus_out` + 鼠标已出本窗即关，窗内点行放行
  （行不可聚焦，点下先 blur，等行自己的 `on_click` 跑命令）。
  开窗 1.5 秒内的抖动 blur 不认（后台呼出时焦点同步会先丢一次，
  认了等于闪退）。
  全局键走 `global-hotkey`（后台也能收），应用内走官方 keybinding（`bind_keys` + Action +
  `on_action`，`None` 上下文后来居上）；两边调同一个开关，系统键会把
  事件吃掉，不双跳。按键录制接键统一走 `shell` 的 `intercept_keystrokes`
  拦截器（见下方「快捷键修改规范」），别在元素上挂 `capture_key_down`。
- `src/shell.rs`——只剩框架：`WsShell`（字段 `pub(crate)`）、`TOOLS`、
  标题栏/侧边栏/分发、`Drop` 兜底。工具视图和设置页视图不在这。
- `src/tray.rs`——系统托盘常驻（不是侧边栏工具）：关窗不退进程，
  点托盘菜单的退出才是真退出（走正常退出钩子：存档/清代理都跑）。
  菜单：显示主窗口 / 截图（后缀展示全局键 `Ctrl+Alt+A`）/ 退出。
  截图键默认 `SHOT_HOTKEY`（微信/QQ 同款），截图页设置 tab 可改，走聚光
  同款 `HotkeyHandle` + 托盘泵轮询，对上 id 才起覆盖层，连按 1s 去抖。
  截图借主窗 WsShell 起区域覆盖层（主窗关了就先重开；`open_window`
  根视图包了层 `Root`，句柄 downcast 不到 WsShell，所以
  `open_main_window` 直接回内容 entity 存全局）。
  事件走通道 + 前台定时轮询（`App::spawn` 的 future 跑在主线程，
  托盘库回调在系统线程，直接碰 `App` 会炸 RefCell，泵里只做
  `try_recv` + `cx.update` 分发）；`TrayIcon` 句柄必须存全局，
  drop 就从托盘消失。主窗句柄记 `MainWindow` 全局，关窗钩子按
  `WindowId` 清掉，唤起时重开不复活僵尸。菜单文案走 `t()`（构建时
  取一次，运行时切语言不重建菜单）。分发逻辑抽纯函数单测。
  平台差异（实测结论，换库前先重读）：mac 菜单栏只认单色模板图
  （`tray-template.png` 白 W，`make-icons.py` 生成，随深浅自动反色；
  Win/Linux 用彩色 32px）；mac 图标挂了菜单后左键必然弹菜单
  （系统行为，左键直唤无解，保持右键菜单）；Windows 左键 Down/Up
  都发事件 + 右键弹菜单，同一套接法左键唤出可用；Linux 缺
  indicator 时建托盘失败只记日志（`libappindicator` 走 dlopen，
  构建期无新增系统依赖，CI 不用加包），无托盘关窗后需重起进程。
  跨平台编译本地验不了（C 依赖缺各平台 SDK 头，`--target` check
  会倒在 ring 上），靠 CI 三平台矩阵兜底，改完托盘看一次 CI 全绿。
- `src/updater/`——自动更新检查（不是侧边栏工具）：启动查一次 GitHub
  最新 Release（`ureq` 阻塞跑后台线程），有新版标题栏右上亮下载图标，
  点即浏览器跳 Release 页。按官方 auto-update 归属表只查不装：
  Inno/DMG/deb 的安装归各平台安装程序管，绝不自替换二进制。
  版本比较只认数字三元组，`parse_version`/`is_newer` 单测覆盖。
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
  **macOS 菜单栏**：系统不许普通窗盖菜单栏，`open_window` 要的全屏矩形
  会被悄悄挪到菜单栏下沿（1x 屏 pos=(0,25)、Retina pos=(0,38)，高度仍
  给足），冻结帧整体下移 → 屏幕上两个菜单栏 + 底部少一截。落位改用
  `Display::bounds()` / `visible_bounds()` 的**顶边差**（`mod.rs` 的
  `overlay_placement`，只认顶边否则 Dock 会被排除出选区），冻结帧按
  `-top_inset` 上移铺满，选区裁剪前用 `select::translate` 补回这段偏移。
  同时 xcap 语义按平台不同（macOS `CGDisplayBounds` 给逻辑点，Windows
  `dmPelsWidth` 给物理像素），`primary_geometry` 统一归一成逻辑点，
  `logical()` 不再二次除缩放——HiDPI 下多除一次覆盖层就缩一半。
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
- `src/tools/airelay/`——AI 转发：本地 OpenAI / Anthropic 兼容网关
  （`127.0.0.1:11459`，端口可改），把请求转到 Zen 免费车道（zen-gate
  核心逻辑的 Rust 移植：会话铸造 + 指纹头 + 工具四件套，三线按模型分流
  chat / responses / messages）。`zen.rs` 纯协议（单测全覆盖，不碰网络）、
  `server.rs` 最小 HTTP/1.1（标准库 TCP，上游走 ureq 阻塞读 SSE）、
  `view.rs` 工具页（开关/端口/模型表/请求日志）。目录走上游
  `GET /zen/v1/models`（10 分钟缓存，失败吃旧的/硬编码兜底）。
  模型页是卡片单选，选中持久化并热换进网关，
  所有请求改写成它（日志里记 `请求→实际`）；没选过默认第一张，
  永远强制，没有跟随模式。请求日志写穿进库（`airelay_requests` 表，
  100 条封顶，重启自动回来），界面是官方 DataTable。
  出站走代理决策（`server.rs`）：先按代理规则判 `opencode.ai`
  （DIRECT/没订阅/没节点 → 裸连，REJECT → 报错）；落节点先走 10809
  （代理工具的隧道在听），否则按组内逐个 pin 私有隧道（10810，
  只听回环，不碰系统代理，随网关启停）。节点连不上或限流（429/
  额度错）自动下一个，上游已吐内容则截断交付不再试；限流额外跳过
  同服节点（同出口 IP 额度是通的），单轮最多 5 次。每次尝试记一行
  （节点名 + 失败原因都记，`egress_node` 列），界面是官方 DataTable。
  阻塞式线程模型，不引入新异步运行时；上游调用扔连接线程，别卡界面。
- `src/tools/coffee/`——咖啡（防休眠）：端起咖啡举系统断言，盖上盖子放行。
  杯子是简约线条手写 SVG，经 resvg 烤成位图（`mod.rs` 的 `cup_svg` +
  `render_cup_rgba`，描边画形、杯身全镂空，唯一的实心是咖啡液；
  tiny-skia 预乘 alpha 要拆一次；`view.rs` 经覆盖层同款 `rgba_to_render_image`
  换 BGRA 直显——整条路零 PNG 编解码，新帧同步上传，不闪；之前每帧换新 PNG
  走异步解码，杯子一直在闪刷。单测锁尺寸/图层/动静/深浅配色差异）：
  开盖是液面 + 三缕蛇形蒸汽（相位错开，上浮 + 摇摆 + 淡入消散，120ms 烤一帧，
  只在开着时转；合盖深浅静态帧各烤一张复用，换主题直接换着显示）。
  配色跟 `cx.theme().mode` 走（深色拿铁色，浅色奶油色），杯子直接摆不套卡片
  （套了像多出来的框），状态是圆点 + 胶囊行（开着主题绿，合着灰）。
  `view.rs` 工具页（杯子 + 开关 + 时长预设 + 屏幕开关 + 自定义分钟）。
  守卫按平台三套：Windows 调 `SetThreadExecutionState`（kernel32 直连，
  `ES_CONTINUOUS` 常驻不用续命）；macOS 挂 `caffeinate -i`（开屏幕开关多挂
  `-d`，不用 `-u` 免得点亮已熄的屏）；Linux 挂 `systemd-inhibit`，
  非 systemd 环境直接报错。子进程 PID 随手入库（`coffee_pid`），正常放行
  （stop/退出）即清；崩了/被杀没清掉，下次启动按 PID 验命令行
  （`caffeinate` / `systemd-inhibit…sleep` 才认，防 PID 重用错杀）认领杀掉，
  隧道旗同款。Windows 断言跟进程走，不可能残留，不记 PID。时长预设 15 分–4 小时 + 常开，开着可换
  （到期点按新时长重定）；自定义 1–1440 分钟。到期自动盖，
  时长和屏幕开关持久化，运行态不恢复（重启默认合盖），退出靠 `Drop` 放行。
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
- flex column 的孩子默认不拉伸（宽度按 max-content 往上冒）：
  `w_full` 的百分比遇到不定宽父级会退化成 auto——要铺满必须给确定像素。
  `flex_1` 的 basis 0 会让换行按 0 算（一行全挤下），别拿它做换行网格。
- `Cargo.toml` 的 `[profile.dev.package]` opt-level 覆盖是刻意的
  （只优化框架依赖，应用自身保持可调试）。`Cargo.lock` 保持提交。
  永远不要提交 `target/`。

## 快捷键修改规范（呼出键 / 截图键）

两处改键（设置页呼出键、截图页设置 tab）共用下面四条，都是实测踩过的坑，
改 gpui-kit / `global-hotkey` 版本后要复查。

- **接键用 `App::intercept_keystrokes`，别用元素上的 `capture_key_down`。**
  后者只跑在**聚焦路径**上，而录制一开，承载按钮的那行就被「按下快捷键…」
  提示行顶掉了（截图键那边还额外 `window.blur()` 扔焦点）。按钮卸载 →
  焦点丢失 → 路径塌回根节点 → 行级/根级监听全哑，**怎么按都录不进**。
  拦截器在窗口级、动作解析之前跑，不看焦点。唯一入口是
  `WsShell::new` 里那个 `hotkey_capture`（两个录制共用，按 `window_id`
  过滤掉面板窗/覆盖层窗），录制期间 `stop_propagation` 顺带挡住
  回车/空格把刚点的按钮再点一遍。`Subscription` 存 `WsShell` 字段，drop 即退订。
- **`GlobalHotKeyManager` 全进程只能有一个。** macOS 走 Carbon 的
  `InstallEventHandler`，第二个 `new()` 直接 `Err`（实测
  `Operation not permitted`）。所以走 `spotlight::shared_manager()`
  （静态 `OnceLock` 常驻，失败不写缓存好让下次重试），**任何地方都别再
  自己 `GlobalHotKeyManager::new()`**。曾经就是聚光/截图各建一个，截图那个
  永远 `None`，改键必报「系统热键不可用」。
- **`rebind` 必须先 `unregister` 旧键。** macOS 同一组合不能重复注册
  （`Unable to register hotkey`），不解就换不回之前用过的键，旧键还白占
  一个全局快捷键。`HotkeyHandle` 还实现了 `Drop` 放键：主窗关掉再开
  会新建句柄注册同一组合，旧的还挂着新句柄就注册不上。
- **应用内绑定 `Keymap::add_bindings` 只追加、没清空 API。** 改键只
  `bind_keys` 新键的话旧键照样呼出面板（实测两键都命中）。必须走
  `rebind_app_keymap`：先给旧键压一条 `Unbind(ToggleSpotlight.name())`
  再绑新键，且 `old != new` 才压（`Unbind` 匹配**后续**绑定，同键自改
  会把刚绑的一起摘掉，键直接失灵）。改键入口：`record_hotkey`（录制）
  和设置页「恢复默认」。全局键换绑成功但入库失败只记日志（`rebind_hotkey`），
  不回滚。

## 卡片网格布局规范（模型 / 节点卡片）

- 版式：`div().grid().grid_cols(grid.cols).gap(px(8.)).w(px(grid.width)).min_w_0()`，
  布局经 `crate::shell::grid_layout(window, sidebar_w)` 拿（`GridLayout { cols, width }`）：
  列数按实时窗宽 `(内容区 + 8) / (232 + 8)` 夹 2..=6，定宽恒等于内容区宽
  （窗宽 − 侧边（216，收起 52）− 32 页边距），resize/收起即换。
  侧边栏宽是动画采样值：`render` 里一帧只采一次（`sidebar_width`），
  sidebar 和网格共用，收起/展开时卡片跟着走（目标驱动 spring，
  连点反转不断，见 https://gpui-kit.com/docs/animation/）。
- 卡片本体不设 flex 和定宽（轨道定宽，设了会被覆盖还乱换行）；
  `window` 从 `render` 经 `render_content` / `render_tool_body` 往下传到网格。
- 宽表（请求 / 连接 DataTable）同理在链上加 `min_w_0`，表格被夹出确定宽度，
  表内横滚才出得来；只加 `min_w_0` 不加宽约束时窄窗依然可能放不下（另行处理）。

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
- 纯图标按钮必须带 hover 提示：`Button` 用 `.tooltip(t("app.xxx"))`
  说明功能（文案同样两列 + `KEYS` 登记）。`div` 没有公开的 tooltip
  接法（框架 `ManagedTooltipExt` 未导出），图标式导航行暂不挂。
  官方 `SidebarToggleButton` 没透出 tooltip，要提示就自建同款。
- `extend!(gpui_component)` 在启动时只调一次，且必须在 `gpui_kit::init`
  之前。语言切换走 `component::set_locale`（经 `i18n::set_language`）。
- 改全局 locale 的测试必须拿 `i18n::lock_locale()` 串行，否则并行跑
  互相覆盖。
- 快捷键改键逻辑要单测能覆盖的部分：`shared_manager` 同一性、`Keymap`
  的 `Unbind` 前后命中数（见 `spotlight` 的两个测试）。录制交互本身
  （点「修改」→ 按组合键）无头驱动不了，别为了测它加 `test-support`，
  靠上面两条纯逻辑兜住回归。
- 语言运行时可切（`set_language` + `cx.notify()` 即时重绘），窗口标题
  等窗口级文字需重启生效；主题用 `theme::apply` 即时切换并重绘所有窗口。
- 设置持久化走 libsql 本地库（`src/settings.rs`，键值表 + 自动规则表
  `proxy_rules_auto`）：Windows 在 `%APPDATA%\WongTools\settings.db`，
  macOS 在 `~/Library/Application Support/WongTools/settings.db`。
  窗口位置（`window_bounds`，含最大化旗）启动恢复、越界居中，
  退出钩子存档；侧边栏选中（`selected_tool`）随点随存。
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
- 提交前必须 `cargo check` 零警告 + `cargo nextest run --locked` 全过
  （没装 nextest 才回落 `cargo test`）。测试挂了先修
  测试或代码，绝不删断言蒙混过关。
