//! Portal 单页 HTML（登录 / 注册 / 列表三态同页）。
//!
//! # 为何不复用 admin-ui
//! `admin-ui` 是完整的 React 应用（构建产物数百 KB，含图表、路由、状态管理）。这个页面
//! 只做三件事：登录、注册、列一张表。挂进那套体系要引入构建依赖和路由耦合，而收益为零。
//! 一段自包含的 HTML + 原生 JS 反而更好：没有构建步骤、没有 npm 依赖、改完即生效，
//! 而且**不与管理面共享任何前端代码**——与「零共享状态」的后端设计一致。
//!
//! # 安全相关的写法约定
//! - 所有来自服务端的字符串一律用 `textContent` 赋值，**绝不用 `innerHTML`**。
//!   key/region/error 都可能含 `<`、`&`，用 innerHTML 就是自找 XSS。
//! - 不把 key 写进 URL、不写 localStorage：前者会进浏览器历史和反代日志，
//!   后者会被任意同源脚本读到。明文只活在当前 DOM 里。
//! - 复制用 `navigator.clipboard`，失败回退到隐藏 textarea + `execCommand`
//!   （HTTP 下 clipboard API 不可用，而内网调试正是 HTTP）。

/// 整页 HTML。用 `r##"..."##` 原样嵌入，随二进制走，无需外部静态文件。
///
/// # 为何是 `r##` 而不是 `r#`
/// HTML 里只要出现 `"#` 这两个字符相邻（`href="#"`、`content="#fff"`、
/// `<a href="#top">`），`r#"…"#` 就会**在那里提前结束**。真实症状是几十个
/// 「prefix `xxx` is unknown」——报错位置在字符串中间，与真正的原因（引号提前
/// 闭合）毫无关系，读报错完全找不到方向。本轮实测踩过。多加一层 `#` 让
/// 终止符变成 `"##`，那个序列在 HTML 里没有自然出现的理由。
pub const PAGE_HTML: &str = r##"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="referrer" content="no-referrer">
<title>Kiro 车队</title>
<style>
:root{--bg:#0f1115;--card:#181b22;--line:#262b36;--fg:#e6e8ec;--dim:#8b93a5;--acc:#4c8dff;--ok:#3fb950;--bad:#f85149}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--fg);font:14px/1.5 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,"Helvetica Neue","PingFang SC","Microsoft YaHei",sans-serif}
/* 内容宽度上限。960 太窄：这一页有 8 列（含上游额度那一格的 5 行堆叠），
   960 下车票和账号都得折行，实测行高 131px；放到 1200+ 之后它们各变回 1 行，
   行高 113px。1400 是收益的拐点——再宽下去行高不再降，只是把内容推得更散。
   1920 屏上两侧仍留 260px 空白，那是刻意的：整行铺满屏幕会让眼睛在列间失去落点。 */
.wrap{max-width:1400px;margin:0 auto;padding:24px 16px}
.card{background:var(--card);border:1px solid var(--line);border-radius:10px;padding:20px}
h1{font-size:18px;margin:0 0 4px}
.sub{color:var(--dim);font-size:12px;margin-bottom:18px}
.auth{max-width:360px;margin:8vh auto}
.tabs{display:flex;gap:4px;margin-bottom:16px}
.tab{flex:1;padding:8px;text-align:center;background:transparent;border:1px solid var(--line);border-radius:6px;color:var(--dim);cursor:pointer;font-size:13px}
.tab.on{background:var(--acc);border-color:var(--acc);color:#fff}
label{display:block;font-size:12px;color:var(--dim);margin:10px 0 4px}
input{width:100%;padding:9px 10px;background:#0f1115;border:1px solid var(--line);border-radius:6px;color:var(--fg);font-size:14px}
input:focus{outline:none;border-color:var(--acc)}
button{cursor:pointer;font-family:inherit}
.btn{width:100%;margin-top:16px;padding:10px;background:var(--acc);border:none;border-radius:6px;color:#fff;font-size:14px}
.btn:disabled{opacity:.5;cursor:not-allowed}
.msg{margin-top:12px;padding:9px 10px;border-radius:6px;font-size:13px;display:none;word-break:break-word}
.msg.err{display:block;background:rgba(248,81,73,.12);border:1px solid rgba(248,81,73,.4);color:#ffb3ae}
.msg.ok{display:block;background:rgba(63,185,80,.12);border:1px solid rgba(63,185,80,.4);color:#8ff0a4}
.top{display:flex;align-items:center;justify-content:space-between;margin-bottom:16px;gap:12px;flex-wrap:wrap}
.who{color:var(--dim);font-size:12px}
.mini{padding:6px 12px;background:transparent;border:1px solid var(--line);border-radius:6px;color:var(--fg);font-size:12px}
.mini:hover{border-color:var(--acc)}
table{width:100%;border-collapse:collapse;font-size:13px}
/* 看板里的表格（车辆热度、失败记录等）仍可能比窄视口宽。让**表格容器**自己
   横滚，而不是让整个页面横滚：页面级横滚会把顶部的余额、按钮一起推出屏幕，
   用户得先左右拖才能点「退出」。
   车队列表本身已不是表格（见 .cars），这一条只服务于看板。 */
.blk{overflow-x:auto}
th{text-align:left;padding:8px;border-bottom:1px solid var(--line);color:var(--dim);font-weight:500;font-size:12px;white-space:nowrap}
td{padding:8px;border-bottom:1px solid var(--line);vertical-align:top}
tr:last-child td{border-bottom:none}
.k{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:12px;word-break:break-all;max-width:380px}
.gone{color:var(--dim);font-style:italic}
/* .sub 的 18px 下边距是给登录卡的副标题用的；表格里那么大间距会把行撑得
   一行比一行高。格内的副行统一压掉边距。 */
td .sub{margin-bottom:0}
/* 计时器：每秒重写文本。等宽数字防止「59秒 → 1分」这类跳变时列宽抖动。 */
.age{font-variant-numeric:tabular-nums}
/* Region 行在 .k 格里，而 .k 带 word-break:break-all——那是给长 key 换行用的，
   套到 region 上就出事：手机窄屏实测把 `ap-northeast-1` 断成了三行
   （`ap-` / `northeast-` / `1`）。
   【为何仅 word-break:normal 不够】它只是不再「任意字符处断行」，连字符处
   仍是合法断点，region 全是 `xx-yyyy-N` 这种形状，于是照样断。必须 nowrap。
   区域名断行不是难看的问题：用户是照着这一行手抄进客户端配置的，
   `ap-` 换行后极易抄成 `ap-northeast` 或漏掉尾号，而配错 region 的报错
   （403/DNS 失败）完全看不出根因。
   【为何 nowrap 只给值、不给整行】把「Region ap-northeast-1」整块设为
   nowrap 会让这一格的最小宽度含上「Region 」这个标签，实测 390 宽下把
   「复制」按钮顶出了可视区——主操作要横滑才点得到，比断行更糟。
   标签与值之间允许换行，值本身不断，两个目标就都成立了。 */
.region{word-break:normal;overflow-wrap:normal;margin-top:2px}
.region b{font-weight:inherit;white-space:nowrap}
/* nowrap 不是装饰：列一多，徽章所在的格被挤窄时「已满」会折成竖排两行
   （实测截图如此）。徽章必须整块不折行。 */
.pill{display:inline-block;padding:1px 7px;border-radius:10px;font-size:11px;border:1px solid;white-space:nowrap}
.pill.ok{color:var(--ok);border-color:rgba(63,185,80,.4)}
.copy{padding:3px 9px;background:transparent;border:1px solid var(--line);border-radius:5px;color:var(--dim);font-size:11px;white-space:nowrap}
.copy:hover{border-color:var(--acc);color:var(--fg)}
/* 【.quota / .quota-main / .quota-track / .quota-fill / .quota-actions 已删】
   它们服务于旧表格里那个「Kiro 上游额度」格（5 行堆叠 + 一条已用进度条）。
   车位卡改用 .fuel 那组画油量，这几个类再没有使用者。
   留着的坏处不是几行样式：下一个人看到 .quota-fill 会以为额度条还在用它，
   于是改了颜色阈值却什么都没变——用不到的样式就是错误的路标。 */
.quota-age{font-variant-numeric:tabular-nums}
.quota-refresh{padding:2px 7px;background:transparent;border:1px solid var(--line);border-radius:5px;color:var(--dim);font-size:11px}
.quota-refresh:hover{border-color:var(--acc);color:var(--fg)}
.quota-refresh:disabled{opacity:.5;cursor:not-allowed}
.empty{text-align:center;padding:40px 0;color:var(--dim)}
.hint{color:var(--dim);font-size:11px;margin-top:14px;line-height:1.7}
.pill.warn{color:#d29922;border-color:rgba(210,153,34,.4)}
/* 汇总条：一行几个小块，窄屏自动折行 */
.sm{display:flex;flex-wrap:wrap;gap:18px;margin-bottom:14px;padding:12px 14px;background:var(--card);border:1px solid var(--line);border-radius:8px}
.sm-item{display:flex;flex-direction:column;gap:2px;min-width:64px}
.sm-label{font-size:11px;color:var(--dim)}
.sm-val{font-size:16px;font-weight:600;font-variant-numeric:tabular-nums}
/* 数字列右对齐 + 等宽数字，让不同行的数位能上下对齐、便于扫视 */
.num{text-align:right;font-variant-numeric:tabular-nums}
/* ---- 车队积分 ---- */
/* 余额条：与汇总条同一视觉层级，但用强调色边框区分「这是我的钱」 */
.wallet{display:flex;flex-wrap:wrap;align-items:baseline;gap:16px;margin-bottom:14px;padding:12px 14px;background:var(--card);border:1px solid rgba(76,141,255,.35);border-radius:8px}
.wallet-bal{font-size:20px;font-weight:600;font-variant-numeric:tabular-nums;color:var(--acc)}
.wallet-meta{font-size:12px;color:var(--dim)}
/* 车费规则说明。比钱包条弱一档（灰边、无强调色）：它是参考信息，
   不该和「我的余额」抢注意力。 */
.rules{margin-bottom:14px;padding:11px 14px;background:var(--card);border:1px solid var(--line);border-radius:8px;font-size:12px;color:var(--dim)}
.rules-h{color:var(--fg);font-size:12px;margin-bottom:7px}
/* 价格表横排，窄屏可横滑。不折行：折了之后「人数」和它对应的「单价」会错位，
   而这张表的全部意义就是这个对应关系。 */
.rules-tbl{display:flex;gap:0;overflow-x:auto;margin:8px 0 2px;font-variant-numeric:tabular-nums}
.rules-col{display:flex;flex-direction:column;min-width:26px;text-align:center;flex:0 0 auto}
.rules-col span{padding:2px 0;white-space:nowrap}
.rules-col .n{color:var(--dim);font-size:11px;border-bottom:1px solid var(--line)}
.rules-col .p{color:var(--fg)}
/* 当前人数所在那一列高亮：让「我现在上车是这个价」一眼可见，不用自己数第几列。 */
.rules-col.on .n{color:var(--acc)}
.rules-col.on .p{color:var(--acc);font-weight:600}
/* 上车按钮。用强调色实心：它是这一页唯一会花钱的操作，得显眼到不会误触。 */
.board{padding:4px 10px;background:var(--acc);border:1px solid var(--acc);border-radius:5px;color:#fff;font-size:11px;white-space:nowrap}
.board:hover{filter:brightness(1.1)}
.board:disabled{opacity:.45;cursor:not-allowed;filter:none}
/* 满员态：去掉强调色，避免一个点不动的按钮还长得像「快来点我」。 */
.board.full{background:transparent;border-color:var(--line);color:var(--dim)}
/* 未上车的车票占位。等宽 + 暗色，视觉上明确「这里有东西但还没解锁」 */
.locked{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:12px;color:var(--dim);letter-spacing:2px}
/* ---- 运营看板（仅管理员可见的第三屏） ---- */
/* 区块。看板有六块内容，.card 没有下边距，直接堆会粘成一整片。 */
.blk{background:var(--card);border:1px solid var(--line);border-radius:10px;padding:16px 18px;margin-bottom:14px}
.blk-h{font-size:13px;color:var(--dim);margin-bottom:12px}
/* .sub 的 18px 下边距是给登录卡副标题用的，区块内的说明行按 td 那套压掉。
   不压的话账目自检那三行说明会被撑开成半屏高。 */
.blk .sub{margin-top:8px;margin-bottom:0}
/* 与 .sm 同一套排版，但不带边框背景——它已经在 .blk 里了，再套一层框是双线。 */
.sm-row{display:flex;flex-wrap:wrap;gap:18px}
.pill.bad{color:var(--bad);border-color:rgba(248,81,73,.4)}
/* ---- 审计（第四屏） ---- */
/* 筛选栏。窄屏自动折行，每个字段自带标签。
   【类名必须与 HTML 逐字一致】第一版这里用的是缩写名，而 HTML 用的是全名，
   两边各起了一套，样式全部静默失效——页面不报错，只是布局是错的。
   两条检查一起把它点了出来：一边报「用了未定义的类」，一边报「定义了没用的类」。
   注意 CSS 注释里**不要写类名形状的字样**：类名提取器不剔 CSS 注释，
   写在这里的示例名会被当成真实定义，于是又冒出一条「定义了没用」。 */
.filters{display:flex;flex-wrap:wrap;gap:10px;align-items:flex-end;margin-bottom:14px;padding:12px 14px;background:var(--card);border:1px solid var(--line);border-radius:8px}
.f-item{display:flex;flex-direction:column;gap:4px}
/* input 的默认 width:100% 会让每个筛选框各占一整行。这里按内容给宽度。
   【为何不用 flex:1】那样几个框会平分整行宽度，时间框被压到看不见日期，
   而用户名框又宽得能装一篇文章。各字段所需宽度本来就不一样。 */
.filters input,.filters select{width:auto;min-width:130px}
.filters label{margin:0}
/* 筛选按钮区。.btn 自带 width:100% 和 margin-top:16px（那是给登录卡的整宽按钮
   用的），放进筛选栏会独占一行并跟其它字段错开一大截。这里覆盖掉。 */
.f-actions{display:flex;gap:8px;align-items:flex-end}
.f-btn{width:auto;margin-top:0;padding:8px 16px;font-size:13px}
/* select 要显式给颜色：不给的话下拉在暗色背景下是「黑底黑字」，
   选项能选但读不出来（实测 Chrome 如此）。 */
select{padding:8px 10px;background:#0f1115;border:1px solid var(--line);border-radius:6px;color:var(--fg);font-size:13px}
/* 分页条 */
.pager{display:flex;align-items:center;gap:10px;margin-top:12px;flex-wrap:wrap}
.p-info{color:var(--dim);font-size:12px;font-variant-numeric:tabular-nums}
/* 动作徽章复用 .pill 那一套配色（pill ok / pill warn / pill bad），
   这里只管等宽字体——动作名是 snake_case 标识符，等宽下更好扫。 */
.pill.mono{font-family:ui-monospace,SFMono-Regular,Menlo,monospace}
/* detail 可能很长（admin 备注是自由文本）。给上限并允许断行，
   否则一条长备注会把整张表撑出横向滚动条。 */
.detail{max-width:320px;word-break:break-word}
@media(max-width:600px){.hide-sm{display:none}
/* 窄屏收紧格内左右留白。Region 值不许折行（见 .region b）给车票列设了一个
   最小宽度，实测 390 宽下把「复制」按钮推到了 399px——只差 9px 就在屏外，
   而那是本页的主操作，得横滑才点得到。5 列 × 左右各 3px 省出 30px，
   比缺口宽出三倍，不靠临界值成立。改 padding 而不是让 region 折行：
   断行后的 `ap-` / `northeast-` 会被用户抄错，代价比留白更大。 */
td,th{padding:8px 5px}}
/* ==================== 车位卡 ====================
   车队列表不再是表格。

   【为何不是表格】表格的前提是「多行同构、要跨行比较同一列」。这一页不是那样：
   用户一次只关心一辆车——还有几个位子、油还剩多少、上车要多少分。而 8 列并排
   带来的是实测数据：390 宽下表格需要 488px 而可用 316px，主操作「复制」被推到
   屏外 130px；桌面端则是「账号/开车时间/车票」三格同时折行，行高 131px。
   两头都在为一个没人用的比较能力付代价。

   卡片把一辆车的信息收在一个边框里，天然自适应：宽屏并排放几张，窄屏叠成一列，
   不需要任何断点去补救「列太多」。 */
/* auto-fill + minmax：宽度够就并排，不够自动减列。
   【为何 min(320px,100%) 而不是 320px】minmax 的下限是硬下限，网格轨道不会
   收缩到它以下。320 视口减去 .wrap 的左右 16px 只剩 288px，写死 320px 会让
   轨道比容器宽 32px——页面级横滚，顶部的余额和退出按钮一起被推出屏幕。
   min(…,100%) 让下限在窄容器里退让到容器宽度，横滚不会发生。 */
.cars{display:grid;grid-template-columns:repeat(auto-fill,minmax(min(320px,100%),1fr));gap:12px}
.car{background:var(--card);border:1px solid var(--line);border-radius:10px;padding:14px 16px;display:flex;flex-direction:column;gap:9px}
/* 自己已上车的车用绿边框标出来。
   【为何不只靠徽章】一屏可能有十几张卡，逐张找那个小徽章很费眼；边框颜色是
   扫一眼就能分辨的层级。徽章仍然保留——颜色不该是唯一的信息载体（色盲用户
   看不出绿边和灰边的区别）。 */
.car.mine{border-color:rgba(63,185,80,.45)}
/* 满员的车整张压暗：它不是错误状态，只是现在没得选，不该和可上车的车抢注意力。 */
.car.full{opacity:.72}
/* 车头：车号 + 套餐 + 徽章一行，发车时间推到最右。 */
.car-h{display:flex;align-items:center;gap:8px;flex-wrap:wrap}
.car-id{font-size:15px;font-weight:600}
.car-plan{font-size:11px;color:var(--dim)}
.car-when{margin-left:auto;font-size:11px;color:var(--dim);font-variant-numeric:tabular-nums}
/* 一行「标签 + 内容」。标签窄且固定宽，让「座位/油量」两行的内容左边缘对齐。 */
.car-row{display:flex;align-items:center;gap:9px;font-size:12px;min-width:0}
.car-lbl{flex:0 0 28px;color:var(--dim);font-size:11px}
/* ---- 座位 ----
   一个点一个位子。数字仍然照给（`2/15`）——点阵适合一眼估量，精确值适合确认。 */
.seats{display:flex;flex-wrap:wrap;gap:3px;flex:1;min-width:0}
.seat{width:9px;height:9px;border-radius:50%;border:1px solid var(--line);flex:0 0 auto}
.seat.on{background:var(--acc);border-color:var(--acc)}
/* 自己那个位子用绿色，和车队积分的「已上车」同色系，不必数第几个点。 */
.seat.me{background:var(--ok);border-color:var(--ok)}
.seat-n{font-size:11px;color:var(--dim);font-variant-numeric:tabular-nums;white-space:nowrap}
/* ---- 油量（上游额度）----
   与座位行同构：一条槽 + 右侧数字。颜色阈值沿用 .quota-fill（70% 黄、90% 红）。 */
.fuel{flex:1;min-width:0;height:6px;background:#262b36;border-radius:3px;overflow:hidden}
/* 没油时**槽本身**染红。
   【为何不能只靠 .fuel-i.bad】条子画的是剩余量，剩余 0 时填充宽度正好是 0%，
   于是最该报警的那辆车反而一点红色都看不到——实测 #803（已用 10,124/10,000）
   的条子是一条纯灰，与「还没读过油量」长得一样。染槽让空条自己带上颜色，
   既保住「条短 = 快没了」的方向，又不让最差状态变成最不显眼的那个。
   透明度压到 .28：它是背景，不该比右边那行红字更抢眼。 */
.fuel.dry{background:rgba(248,81,73,.28)}
.fuel-i{height:100%;background:var(--ok);border-radius:3px}
.fuel-i.warn{background:#d29922}
.fuel-i.bad{background:var(--bad)}
.fuel-n{font-size:11px;color:var(--dim);font-variant-numeric:tabular-nums;white-space:nowrap}
/* 没油了：红字点明后果。「剩余 0」本身不够——用户要的是「这辆车现在用不了」。 */
.fuel-n.bad{color:var(--bad)}
/* 油量下面那行小字：重置时间 + 更新时间 + 刷新按钮。 */
.car-meta{display:flex;align-items:center;gap:8px;flex-wrap:wrap;font-size:11px;color:var(--dim);padding-left:37px}
.car-meta .quota-age{font-variant-numeric:tabular-nums}
/* ---- 车票 ----
   与卡片其余部分用一条细线隔开：上半是「这辆车怎么样」，下半是「我的东西」。 */
.car-tkt{border-top:1px solid var(--line);padding-top:9px;display:flex;flex-direction:column;gap:6px}
/* key 用等宽 + 任意处断行：它是照抄进配置文件的，宁可断行也不能溢出。 */
.car-key{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:12px;word-break:break-all;line-height:1.45}
/* 「你在 X 上车」。比车票弱一档：它是事后确认信息，不该和 key 抢注意力。 */
.car-mine{font-size:11px;color:var(--dim);font-variant-numeric:tabular-nums}
/* 底排：车费在左，主操作按钮在最右。
   【为何按钮靠右】它是唯一会花钱的操作，固定在同一个位置能形成肌肉记忆，
   也不会因为左侧车费文案长短不同而左右浮动。 */
.car-foot{display:flex;align-items:center;gap:10px;flex-wrap:wrap}
.car-fee{font-size:12px;color:var(--dim)}
.car-fee b{color:var(--fg);font-weight:600;font-variant-numeric:tabular-nums}
.car-foot .board,.car-foot .copy{margin-left:auto}
/* 窄屏：标签列会挤掉座位点的空间，改成上下堆叠。
   实测 320 宽下 15 个点 + 标签 + 数字放不进一行，点阵会折成两行且与标签错位。 */
@media(max-width:420px){
  .car-row{flex-direction:column;align-items:flex-start;gap:4px}
  .car-lbl{flex:none}
  .seats{width:100%}
  /* 【为何这里必须重置 flex】.fuel 在横排时是 `flex:1`（= flex-basis:0）。
     这个断点把 .car-row 改成 column，于是 flex-basis 作用在**高度**上，
     6px 的槽被压成 0——实测 390 宽下油量条整条消失，只剩右边一个数字，
     而桌面端完全正常。改成 flex:none 让 height 重新生效。 */
  .fuel{flex:none;width:100%}
  .car-meta{padding-left:0}
}
</style>
</head>
<body>
<div class="wrap">

  <div id="view-auth" class="auth" style="display:none">
    <div class="card">
      <h1>Kiro 车队</h1>
      <div class="sub">登录后可查看车队里的号，上车即可拿到车票</div>
      <div class="tabs">
        <button class="tab on" id="tab-login">登录</button>
        <button class="tab" id="tab-reg">注册</button>
      </div>
      <form id="form-auth" autocomplete="off">
        <label for="u">用户名</label>
        <input id="u" name="username" autocomplete="username" autocapitalize="none" spellcheck="false">
        <label for="p">密码</label>
        <input id="p" name="password" type="password" autocomplete="current-password">
        <div id="wrap-code" style="display:none">
          <label for="c">注册码</label>
          <input id="c" name="inviteCode" type="password" autocomplete="off">
        </div>
        <button class="btn" id="go" type="submit">登录</button>
      </form>
      <div class="msg" id="m-auth"></div>
      <div class="hint" id="hint-reg" style="display:none">
        密码至少 10 位，不能是纯数字或纯字母。<br>没有注册码请联系管理员。
      </div>
    </div>
  </div>

  <div id="view-list" style="display:none">
    <div class="top">
      <div>
        <h1 style="display:inline">Kiro 车队</h1>
        <span class="who" id="who"></span>
      </div>
      <div style="display:flex;gap:8px">
        <!-- 看板与审计入口。默认 display:none，enter() 里按 canManage 一起打开——
             对非管理员画出来只会点出一个 403。 -->
        <button class="mini" id="to-dash" style="display:none">看板</button>
        <button class="mini" id="to-audit" style="display:none">审计</button>
        <button class="mini" id="reload">刷新</button>
        <button class="mini" id="logout">退出</button>
      </div>
    </div>
    <!-- 余额条容器。积分未启用时 renderWallet 会把它隐藏（display:none），
         而不是留一条写着「余额 0 分」的空壳——那会让没开积分的部署看起来
         像是所有人都没钱。 -->
    <div class="wallet" id="wallet" style="display:none"></div>
    <!-- 汇总条容器。renderSummary 往这里填；缺了它会让 render 在第一行就
         抛 TypeError（null.textContent），而异常被 loadKeys 的 catch 收敛成
         「网络错误」——真正的原因反而被兜底文案藏住。实测踩过。 -->
    <div class="sm" id="summary"></div>
    <!-- 车费规则说明。积分未启用时 renderRules 隐藏它——那种部署不存在车费，
         摆一份不生效的规则只会让人照着算一遍再发现对不上。内容全部由服务端
         下发的 pricing 渲染，前端不复算公式（见 renderRules 的注释）。 -->
    <div class="rules" id="rules" style="display:none"></div>
    <div class="card">
      <div id="list"></div>
    </div>
    <div class="msg" id="m-list"></div>
  </div>

  <!-- 运营看板。第三屏，只有 canManage 的账号能进（按钮本身也只对他们渲染）。
       【为何是独立一屏而不是列表页上方的一段】看板有六块内容，塞进列表页会把
       「哪辆车能上」挤到首屏之外，而那是绝大多数访问者唯一关心的东西。 -->
  <div id="view-dash" style="display:none">
    <div class="top">
      <div>
        <h1 style="display:inline">运营看板</h1>
        <span class="who" id="dash-when"></span>
      </div>
      <div style="display:flex;gap:8px">
        <button class="mini" id="dash-reload">刷新</button>
        <button class="mini" id="dash-back">返回车队</button>
      </div>
    </div>
    <!-- 账目自检放最前面。
         【为何不放最后】它回答的是「下面这些数字可不可信」。放在末尾的话，
         人已经把六块数字读完并做了判断，才看到一行「账目异常」。 -->
    <!-- 提示放在区块**之前**：读取失败时六个区块会被整体隐藏（见 loadDash），
         那时这条消息就是这一屏唯一的内容，理应在最上面而不是在一片空白下面。 -->
    <div class="msg" id="m-dash"></div>
    <!-- 六个区块套一层容器，为的是读取失败时能一次性隐藏。
         【为何非隐藏不可】.blk 自带边框和内边距，空着也占一整块高度——实测
         关掉积分开关后这一屏是「一句读取失败 + 六个空框」，看起来像页面崩了，
         而真实情况只是一个配置开关没开。 -->
    <div id="dash-body">
      <div class="blk" id="dash-integrity"></div>
      <div class="blk" id="dash-today"></div>
      <div class="blk" id="dash-total"></div>
      <div class="blk" id="dash-tiers"></div>
      <div class="blk" id="dash-keys"></div>
      <div class="blk" id="dash-fails"></div>
    </div>
  </div>

  <!-- 审计。第四屏，同样只有 canManage 能进。
       【为何与看板分屏而不做成看板的第七个区块】看板回答「现在怎么样」，是一屏
       扫完的聚合数；审计回答「谁在什么时候做了什么」，要翻页、要筛、要导出，
       是一次可能坐十分钟的调查。把一张会翻二十页的表塞进看板，会让上面那六块
       聚合数字永远滚到看不见的地方。 -->
  <div id="view-audit" style="display:none">
    <div class="top">
      <div>
        <h1 style="display:inline">审计</h1>
        <span class="who" id="audit-count"></span>
      </div>
      <div style="display:flex;gap:8px">
        <!-- 导出是 <a> 而不是 <button>：浏览器原生的下载行为不需要 JS 参与，
             也就不存在「把 CSV 读进内存再拼 blob」那一步（几千行时那一步会卡）。
             href 由 JS 按当前筛选条件重写，见 syncExportHref。 -->
        <a class="mini" id="audit-export" href="#" download>导出 CSV</a>
        <button class="mini" id="audit-reload">刷新</button>
        <button class="mini" id="audit-to-dash">看板</button>
        <button class="mini" id="audit-back">返回车队</button>
      </div>
    </div>

    <!-- 筛选条。
         【为何用 form 包起来】回车提交是这类筛选框的默认预期；不包 form 的话
         用户在输入框里按回车什么都不会发生，只能去点按钮。 -->
    <form class="filters" id="audit-filters" autocomplete="off">
      <div class="f-item">
        <label for="f-user">用户名（精确）</label>
        <input id="f-user" autocapitalize="none" spellcheck="false" placeholder="留空 = 全部">
      </div>
      <div class="f-item">
        <label for="f-action">动作</label>
        <!-- 下拉的选项由 /audit/actions 填充：只列**实际发生过**的动作，
             并带上次数。写死一份清单的话，新增动作不会出现在这里。 -->
        <select id="f-action"></select>
      </div>
      <!-- 动作族筛选。下拉只能选**一个精确动作**，而审计最常问的问题之一是
           「所有登录失败」——那分散在 login_fail_bad_password /
           login_fail_unknown_user / login_fail_throttled / login_fail_disabled
           四个值里，靠下拉要来回切四次。前缀 login_fail 一次覆盖整族。
           【为何与下拉并存而不取代它】精确值能一步选中且带次数，是最常用的路径；
           前缀是给「按族看」用的。服务端两个字段可以同时生效（AND）。 -->
      <div class="f-item">
        <label for="f-prefix">动作前缀</label>
        <input id="f-prefix" autocapitalize="none" spellcheck="false"
               placeholder="如 login_fail">
      </div>
      <div class="f-item">
        <label for="f-since">起始时间</label>
        <input id="f-since" type="datetime-local">
      </div>
      <div class="f-item">
        <label for="f-until">结束时间</label>
        <input id="f-until" type="datetime-local">
      </div>
      <div class="f-item">
        <label for="f-page">每页</label>
        <select id="f-page">
          <option value="20">20</option>
          <option value="50" selected>50</option>
          <option value="100">100</option>
          <option value="200">200</option>
        </select>
      </div>
      <div class="f-actions">
        <button class="btn f-btn" id="f-apply" type="submit">筛选</button>
        <button class="mini" id="f-clear" type="button">清空</button>
      </div>
    </form>

    <div class="msg" id="m-audit"></div>
    <div class="card">
      <div id="audit-list"></div>
    </div>
    <!-- 翻页条。总数为 0 时整条隐藏（没有东西可翻）。 -->
    <div class="pager" id="audit-pager" style="display:none">
      <button class="mini" id="p-prev">上一页</button>
      <span class="p-info" id="p-info"></span>
      <button class="mini" id="p-next">下一页</button>
    </div>
  </div>

</div>
<script>
'use strict';
var $ = function(id){ return document.getElementById(id); };
var mode = 'login';

function say(el, text, kind){
  el.className = 'msg ' + (kind || 'err');
  el.textContent = text;
}
function clearSay(el){ el.className = 'msg'; el.textContent = ''; }

function show(which){
  $('view-auth').style.display = (which === 'auth') ? 'block' : 'none';
  $('view-list').style.display = (which === 'list') ? 'block' : 'none';
  $('view-dash').style.display = (which === 'dash') ? 'block' : 'none';
  $('view-audit').style.display = (which === 'audit') ? 'block' : 'none';
  // 回到登录屏就停表。
  //
  // 【为何放在这里而不是每个 401 分支】回登录屏有四条路径：主动退出、
  // loadKeys 拿到 401、board 拿到 401、启动时 me 失败。在每处各写一遍
  // stopTimers 迟早漏一条，而漏掉的表现是：会话过期后轮询还在每 10 秒
  // 打一次 /keys，每次都 401 又调 show('auth')——用户正在重输密码，
  // 界面被反复重置。show() 是这四条路径的唯一收口。
  if (which === 'auth') { stopTimers(); }
}

function setMode(next){
  mode = next;
  var isReg = (next === 'register');
  $('tab-login').className = isReg ? 'tab' : 'tab on';
  $('tab-reg').className = isReg ? 'tab on' : 'tab';
  $('wrap-code').style.display = isReg ? 'block' : 'none';
  $('hint-reg').style.display = isReg ? 'block' : 'none';
  $('go').textContent = isReg ? '注册' : '登录';
  $('p').setAttribute('autocomplete', isReg ? 'new-password' : 'current-password');
  clearSay($('m-auth'));
}

function api(path, opts){
  var o = opts || {};
  o.credentials = 'same-origin';
  o.headers = o.headers || {};
  if (o.body) { o.headers['Content-Type'] = 'application/json'; }
  return fetch('/portal/api/' + path, o).then(function(r){
    return r.json().catch(function(){ return {}; }).then(function(j){
      return { ok: r.ok, status: r.status, body: j };
    });
  });
}

// 时间戳 -> 本地时间字符串。服务端只发 Unix 毫秒，时区在浏览器侧决定，
// 避免服务端按 UTC 渲染、用户按本地时间理解而对不上。
function ts(ms){
  if (!ms) { return '-'; }
  var d = new Date(ms);
  var p = function(n){ return (n < 10 ? '0' : '') + n; };
  return d.getFullYear() + '-' + p(d.getMonth()+1) + '-' + p(d.getDate())
       + ' ' + p(d.getHours()) + ':' + p(d.getMinutes());
}

// 「多久以前」。从 addedAtMs 到现在的时长，每秒走一次。
//
// 【为何秒级只在一分钟内显示】超过一分钟后还逐秒跳动的数字没人会读，只会让整页
// 每秒重绘一次。一分钟以内显示秒（刚加的号能看到它在走），之后降级到分/时/天。
//
// 【为何允许负值走到「刚刚」】服务端时钟比浏览器快几秒时差值会是负数，
// 显示「-3 秒」会让人以为数据坏了。钳到 0 当「刚刚」处理。
function ago(ms){
  if (!ms) { return ''; }
  var d = Date.now() - ms;
  if (d < 0) { d = 0; }
  var s = Math.floor(d / 1000);
  if (s < 60) { return s + ' 秒'; }
  var m = Math.floor(s / 60);
  if (m < 60) { return m + ' 分 ' + (s % 60) + ' 秒'; }
  var h = Math.floor(m / 60);
  if (h < 24) { return h + ' 小时 ' + (m % 60) + ' 分'; }
  var day = Math.floor(h / 24);
  return day + ' 天 ' + (h % 24) + ' 小时';
}

// 复制。优先 clipboard API；HTTP 下它不可用（内网调试恰好就是 HTTP），
// 回退到隐藏 textarea + execCommand。
function copy(text, btn){
  var done = function(){
    var old = btn.textContent;
    btn.textContent = '已复制';
    setTimeout(function(){ btn.textContent = old; }, 1200);
  };
  if (navigator.clipboard && navigator.clipboard.writeText) {
    navigator.clipboard.writeText(text).then(done, function(){ fallback(text, done); });
  } else {
    fallback(text, done);
  }
}
function fallback(text, done){
  var ta = document.createElement('textarea');
  ta.value = text;
  ta.setAttribute('readonly', '');
  ta.style.position = 'fixed';
  ta.style.left = '-9999px';
  document.body.appendChild(ta);
  ta.select();
  try { document.execCommand('copy'); done(); } catch (e) { /* 复制失败就让用户手动选 */ }
  document.body.removeChild(ta);
}

// 数字格式化：千分位、token 转 k/M、credits 两位小数。
function num(n){
  if (n === undefined || n === null) return '0';
  return String(n).replace(/\B(?=(\d{3})+(?!\d))/g, ',');
}

// 上游额度允许小数，最多保留两位；车队积分仍走 num() 的整数展示。
// 油量数字。**向下取整**，不留小数。
//
// 【为何不留小数】上游给的是 9372.12 这种值，而这一行回答的问题是「还能不能跑」，
// 小数位对这个判断没有任何增量。实测截图里五辆车只有一辆带 `.12`，那两位数字
// 唯一的作用是让这一列看起来参差不齐、把注意力吸到最不重要的地方。
//
// 【为何是 floor 而不是 round】四舍五入会把 9371.6 报成 9372——把「还剩多少」
// 说多了。油量是个额度，向上抹平等于替上游许诺它没有的量；向下取整最坏是少说
// 一点，用户不会因此多跑一次失败的请求。
//
// 【为何仍走 toLocaleString】千分位分隔符要留：9372 和 93720 差一个数量级，
// 不分组的话得逐位数字数，而这一眼本来就是用来粗判的。
function quotaNum(n){
  var v = Number(n);
  if (!isFinite(v)) { return '—'; }
  return Math.floor(v).toLocaleString('zh-CN');
}

function quotaAge(cachedAt){
  if (!cachedAt) { return '更新时间未知'; }
  return '更新于 ' + ago(Number(cachedAt) * 1000) + '前';
}

// 油量（上游额度）。填进车位卡里的一个容器：一行「油量 [槽] 剩余 X」，
// 下面一行小字（重置时间 · 更新于 X · 刷新）。
//
// 【为何叫油量】这一页整套说法是拼车。用户要判断的是「这辆车还能不能跑」，
// 油量条的形状本身就在回答；精确数字仍照给在右侧和小字里。
//
// 【为何条子画剩余而不是已用】旧表格画的是已用，于是快没额度的车显示一条
// 几乎填满的红条——「满」在直觉上是好事，而那其实是最差的状态。改成剩余后，
// 条子短 = 快没了，方向与直觉一致。
function fillFuel(box, it){
  box.textContent = '';
  var q = it.upstreamQuota;

  var row = document.createElement('div');
  row.className = 'car-row';
  var lbl = document.createElement('span');
  lbl.className = 'car-lbl';
  lbl.textContent = '油量';
  row.appendChild(lbl);

  if (q) {
    var pctValue = Number(q.usagePercentage || 0);
    var used = Math.max(0, Math.min(100, isFinite(pctValue) ? pctValue : 0));
    // 没油了直接说后果。「剩余 0」不够——用户要知道的是「这辆车现在跑不了」。
    //
    // 【为何在画条子之前就算】槽和右边的数字都要用它：剩余 0 时填充宽度是 0%，
    // 光靠填充上不了色，得让**槽本身**染红（见 CSS 里 .fuel.dry 那段）。
    var dry = used >= 100 || Number(q.remaining) <= 0;
    var track = document.createElement('div');
    track.className = 'fuel' + (dry ? ' dry' : '');
    var fill = document.createElement('div');
    fill.className = 'fuel-i' + (used >= 90 ? ' bad' : (used >= 70 ? ' warn' : ''));
    // 宽度取**剩余**比例，用 100-used 算而不是 remaining/limit：后者在超额
    // （实测 #803 是已用 10,124 / 上限 10,000）时会算出负宽度。
    fill.style.width = (100 - used) + '%';
    track.appendChild(fill);
    row.appendChild(track);

    var n = document.createElement('span');
    n.className = 'fuel-n' + (dry ? ' bad' : '');
    n.textContent = dry ? '没油了' : ('剩余 ' + quotaNum(q.remaining));
    row.appendChild(n);
  } else {
    var unk = document.createElement('span');
    unk.className = 'fuel-n';
    unk.textContent = '油量未知';
    row.appendChild(unk);
  }
  box.appendChild(row);

  // 小字行：重置时间 · 更新于 X · 刷新按钮。
  //
  // 【为何刷新按钮和「更新于」挨在一起】那个时间就是用户判断「要不要刷」的
  // 唯一依据。分开摆的话，他得先在别处找到时间、再回来找按钮。
  var meta = document.createElement('div');
  meta.className = 'car-meta';
  if (q) {
    var bits = [];
    if (q.nextResetAt) { bits.push('重置 ' + ts(Number(q.nextResetAt) * 1000)); }
    if (q.overageEnabled) { bits.push('含超额额度'); }
    if (bits.length) {
      var b = document.createElement('span');
      b.textContent = bits.join(' · ');
      meta.appendChild(b);
    }
    var age = document.createElement('span');
    age.className = 'quota-age';
    age.setAttribute('data-cached-at', String(q.cachedAt || ''));
    age.textContent = quotaAge(q.cachedAt);
    meta.appendChild(age);
  } else {
    var none = document.createElement('span');
    none.textContent = '还没读过这辆车的油量';
    meta.appendChild(none);
  }
  if (it.canRefreshUpstreamQuota) {
    var refresh = document.createElement('button');
    refresh.className = 'quota-refresh';
    refresh.textContent = '刷新';
    refresh.title = '从 Kiro 上游重新读取这张凭据的真实额度';
    refresh.addEventListener('click', function(){ refreshQuota(it, refresh, box); });
    meta.appendChild(refresh);
  }
  if (meta.childNodes.length) { box.appendChild(meta); }
}

// 刷新一辆车的油量。
//
// 【为何失败时要恢复按钮文字、成功时不用】成功走的是 fillFuel(box, it)，整个
// 油量区连按钮一起重建，btn 那一刻已脱离文档——再去改它的文字是改一个看不见的
// 节点。失败时容器没重建，那个「刷新中…」的按钮还在原地，不恢复就永久卡住。
//
// 【box 而不是 td】容器现在是车位卡里的油量区（.car-fuel），不再是表格单元格。
// 传错的后果是刷新后油量区空掉——fillFuel 第一步就是清空容器。
function refreshQuota(it, btn, box){
  btn.disabled = true;
  btn.textContent = '刷新中…';
  clearSay($('m-list'));
  QUOTA_BUSY++;
  api('keys/' + encodeURIComponent(it.id) + '/balance/refresh', { method: 'POST' }).then(function(r){
    QUOTA_BUSY = Math.max(0, QUOTA_BUSY - 1);
    if (r.status === 401) { show('auth'); return; }
    if (!r.ok) {
      say($('m-list'), (r.body && r.body.error) || '上游额度刷新失败');
      btn.disabled = false;
      btn.textContent = '刷新';
      return;
    }
    it.upstreamQuota = r.body;
    fillFuel(box, it);
    say($('m-list'), '油量已刷新', 'ok');
  }).catch(function(err){
    QUOTA_BUSY = Math.max(0, QUOTA_BUSY - 1);
    console.error('额度刷新异常:', err);
    btn.disabled = false;
    btn.textContent = '刷新';
    var isNet = err instanceof TypeError && /fetch|network|Failed/i.test(String(err && err.message));
    say($('m-list'), isNet ? '网络错误，请重试' : ('额度刷新出错：' + (err && err.message ? err.message : String(err))));
  });
}

// 【这里原先还有 tok / pct / cdText / rfc / statusLabel / statusClass / spark】
// 它们只服务于已删掉的运维列（Tokens、成功率、冷却倒计时、最后使用、状态、走势）。
// 留着不删的坏处不是浪费几行：下一个人看到 statusClass 会以为这一页仍在展示凭据
// 健康状态，从而按那个前提改代码。用不到的辅助函数就是错误的路标。
// 需要这些展示的是凭据管理页，那边有自己的实现。

// 余额条。积分未启用时整条隐藏。
//
// 【为何不在未启用时显示「余额 0 分」】那会让没开积分的部署看起来像所有人都欠费，
// 而实际上那种部署根本不看积分。隐藏比显示一个无意义的 0 更准确。
function renderWallet(w, creditsOn, aboardCount){
  var box = $('wallet');
  box.textContent = '';
  if (!creditsOn) { box.style.display = 'none'; return; }
  box.style.display = 'flex';

  var bal = document.createElement('div');
  bal.className = 'wallet-bal';
  bal.textContent = num(w && w.balance) + ' 分';
  box.appendChild(bal);

  var meta = document.createElement('div');
  meta.className = 'wallet-meta';
  var bits = ['已上 ' + num(aboardCount) + ' 辆车'];
  if (w && w.spent) { bits.push('累计消费 ' + num(w.spent) + ' 分'); }
  if (w && w.topup) { bits.push('累计充值 ' + num(w.topup) + ' 分'); }
  meta.textContent = bits.join(' · ');
  box.appendChild(meta);

  // 余额为 0 时给一句明确指引。不给的话用户只看到按钮点不动，不知道该找谁。
  if (!w || !w.balance) {
    var tip = document.createElement('div');
    tip.className = 'wallet-meta';
    tip.textContent = '余额不足，请联系管理员充值';
    box.appendChild(tip);
  }
}

// 车费规则说明。
//
// 【为何整块由服务端下发、前端一个数都不算】车费公式是两段式 + ceil 取整 + min
// 钳制，前端复算一遍就等于有了第二份实现。那两份实现只要有一处不同步（改了
// base_count、加了新钳制），页面上写的价和真正扣的分就会不一致——而用户只相信
// 自己看到的那个，事后没人能解释为什么扣多了。所以连价格表都用服务端算好的
// priceTable，这里只负责把数字摆出来。
//
// 【为何不展示 totalPrice】它叫「总价」但**不是**「一把车总共只收这么多」：
// 触到下限后实收随人数线性涨（15 人 × 3 分 = 45 分 > 20）。把它摆在用户眼前
// 只会让人拿它当总额去核对，然后发现对不上。这里只讲能直接对上的三件事：
// 前几人多少钱、之后怎么摊、最低到多少。
function renderRules(p, creditsOn, items){
  var box = $('rules');
  box.textContent = '';
  if (!creditsOn || !p) { box.style.display = 'none'; return; }
  box.style.display = 'block';

  var h = document.createElement('div');
  h.className = 'rules-h';
  h.textContent = '车费规则';
  box.appendChild(h);

  // 文字规则。三句话对应三个可验证的事实，都直接取自服务端字段。
  var line = document.createElement('div');
  var seg = [];
  seg.push('前 ' + num(p.baseCount) + ' 人每人 ' + num(p.basePrice) + ' 分');
  seg.push('之后按人数均摊，人越多越便宜');
  seg.push('最低 ' + num(p.minPrice) + ' 分/人');
  seg.push('一辆车最多 ' + num(p.maxBoarders) + ' 人');
  line.textContent = seg.join(' · ');
  box.appendChild(line);

  // 价格表。横向一行，窄屏可横滑（.rules-tbl 有 overflow-x）。
  var tbl = document.createElement('div');
  tbl.className = 'rules-tbl';
  var tab = p.priceTable || [];
  // 当前车队里人最多的那辆车，用来在表上高亮「现在走到第几个位子」。
  // 【为何取最大值而不是某一行】这块说明是全局的，不属于某一辆车；取最大值
  // 回答的是「这个车队现在最热的一辆走到哪了」，这也是用户最关心的那个数。
  var cur = 0;
  (items || []).forEach(function(it){
    if (it.boardCount && it.boardCount > cur) { cur = it.boardCount; }
  });
  tab.forEach(function(price, i){
    var n = i + 1;
    var col = document.createElement('div');
    col.className = 'rules-col' + (n === cur ? ' on' : '');
    var top = document.createElement('span');
    top.className = 'n';
    top.textContent = n;
    col.appendChild(top);
    var bot = document.createElement('span');
    bot.className = 'p';
    bot.textContent = price;
    col.appendChild(bot);
    tbl.appendChild(col);
  });
  box.appendChild(tbl);

  var foot = document.createElement('div');
  foot.className = 'sub';
  // 差额退款是这套规则里最容易被误解的一环：先上车的人看到自己余额「变多了」，
  // 不解释的话会当成 bug 来问。说清「你永远只付当前那个价」比列公式有用。
  foot.textContent = '上排为人数，下排为每人车费。后来的人上车后，先上车的人会自动退回差额——'
    + '任何时候你的实付都等于当前人数对应的那个价。';
  box.appendChild(foot);
}

// 车票区：已上车给明文 + Region，未上车给遮盖串 + 指纹。
//
// 遮盖串是**前端的呈现**，不是安全边界——未上车时服务端根本没下发明文
// （见 http.rs 的 gate_plaintext）。这里显示 •••• 只是为了让「有东西但要买」
// 这件事看起来合理，而不是显示一片空白让人以为这号坏了。
function fillTicket(td, it){
  td.textContent = '';
  td.className = 'car-key';
  if (it.key) {
    var kv = document.createElement('div');
    kv.textContent = it.key;
    td.appendChild(kv);
    // Region 必须和车票一起给：只有 key 的用户配不出能用的 client——
    // 网关按凭据的 region 打上游，客户端配错 region 就是一路 403/DNS 失败，
    // 而错误信息里完全看不出「region 不对」。这一行是服务端算出的**实际生效值**
    // （与网关同一个函数，见 http.rs 的 effective_upstream_region）。
    if (it.region) {
      var rg = document.createElement('div');
      rg.className = 'sub region';
      // 标签与值分成两个节点：只有值那个不许折行（见 .region b）。
      // 【为何不用 innerHTML 拼】region 虽已过白名单、不可能含 `<`，但本页的规矩是
      // 全程 createElement/textContent——一旦这里开了 innerHTML 的口子，下一个
      // 照抄这段的字段就未必过白名单了。
      rg.appendChild(document.createTextNode('Region '));
      var rv = document.createElement('b');
      rv.textContent = it.region;
      rg.appendChild(rv);
      td.appendChild(rg);
    }
    return;
  }
  if (it.keyKind === 'oauth') {
    td.className = 'car-key gone';
    td.textContent = '网页登录账号（无车票可复制）';
    return;
  }
  // 只读角色：服务端已把明文裁掉（keyKind='forbidden'），这里给一句说明。
  //
  // 【为何必须显式处理这一档】漏掉它的后果不是报错，而是落进本函数最后那个
  // 默认分支去显示 `maskedKey`——打码值本身不算泄漏（前 8 + 后 4，中间省略，
  // 复原不出真实 key），但那个值对同前缀的号**长得一模一样**（本项目的演示号
  // 全是 `ksk_DEMO...cdef`），于是只读账号看到的是五行完全相同的字符串，
  // 且指纹也丢了——既没信息又像坏了。实测截图正是如此发现的。
  if (it.keyKind === 'forbidden') {
    var ro = document.createElement('span');
    ro.className = 'locked';
    ro.textContent = '••••••••••••';
    td.appendChild(ro);
    var rf = document.createElement('div');
    rf.className = 'sub';
    // 指纹照给：只读账号的用途是核对与审计，「这把是不是我手上那把」正是它要答的问题。
    rf.textContent = it.fingerprint ? ('指纹 ' + it.fingerprint + ' · 只读账号无权查看') : '只读账号无权查看';
    td.appendChild(rf);
    return;
  }
  if (it.keyKind === 'locked') {
    var m = document.createElement('span');
    m.className = 'locked';
    m.textContent = '••••••••••••';
    td.appendChild(m);
    // 指纹在未上车时也给，让人能在付费前确认「这把是不是我要的那把」。
    if (it.fingerprint) {
      var f = document.createElement('div');
      f.className = 'sub';
      f.textContent = '指纹 ' + it.fingerprint;
      td.appendChild(f);
    }
    return;
  }
  td.className = 'car-key gone';
  td.textContent = it.maskedKey || '（取不到车票）';
}

// 座位行：一个点一个位子，已占的填色，自己那个用绿色，右侧仍给精确数字。
//
// 【为何必须是独立函数、且整块重建】上车成功后要把这一行从 `2/15` 更新成 `3/15`
// 并点亮一个点。旧表格那一格只有文字，board() 直接 `textContent = ...` 就够了；
// 换成点阵后同样的赋值会**把所有点子一起抹掉**，只剩一行数字——而扣分是成功的，
// 用户看到的是「付了钱，座位图消失了」。所以 board() 改为调用本函数重建整行。
//
// 【为何不复用 fillFuel 的写法把标签也画进来】座位与油量共用 .car-row 的排版，
// 但座位在上车后要重画、油量在刷新后要重画，两者时机不同。各自负责自己那一行，
// 重建时不会牵动另一行（否则刷新油量会让座位图闪一下）。
function fillSeats(box, it){
  box.textContent = '';
  box.className = 'car-row';

  var lbl = document.createElement('span');
  lbl.className = 'car-lbl';
  lbl.textContent = '座位';
  box.appendChild(lbl);

  // OAuth 号不参与车队：没有座位概念，给一句说明而不是画 0/N 的空座位图
  // （后者看起来像「还没人上，快来」，而这辆车根本上不了）。
  if (it.keyKind === 'oauth') {
    var na = document.createElement('span');
    na.className = 'seat-n';
    na.textContent = '网页登录账号，不参与拼车';
    box.appendChild(na);
    return;
  }

  var max = Number(it.maxBoarders) || 0;
  var taken = Math.max(0, Number(it.boardCount) || 0);

  var seats = document.createElement('div');
  seats.className = 'seats';
  // 上限兜底：maxBoarders 理论上是服务端配置的小数字（本部署 15），但若哪天
  // 配成了几百，画几百个点会把卡片撑成一整屏。超过 30 就只给数字，不画点阵。
  //
  // 【为何是画不画的取舍而不是缩小点子】点阵的意义是「一眼估量还剩几个位子」，
  // 点子小到 3px 就已经数不清了，那时数字才是唯一有效的呈现。
  if (max > 0 && max <= 30) {
    for (var i = 0; i < max; i++) {
      var s = document.createElement('span');
      // 自己的位子画在已占的最后一个：服务端不下发「我是第几个上车的」，
      // 而那个序号对用户也没有意义——他只需要知道「里面有一个是我」。
      var mine = it.aboard && i === taken - 1;
      s.className = 'seat' + (i < taken ? (mine ? ' on me' : ' on') : '');
      seats.appendChild(s);
    }
    box.appendChild(seats);
  }

  var n = document.createElement('span');
  n.className = 'seat-n';
  n.textContent = num(taken) + '/' + num(max) + ' 位';
  box.appendChild(n);
}

// 状态徽章（已满 / 已上车）画在**车头**，不在座位行。
//
// 【为何从座位行搬走】实测：徽章挤在座位行里会把点阵顶到折行——15 个点 + 标签 +
// 「2/15 位」+ 徽章需要 332px，而 1920 宽下四列布局每张卡内宽只有 301px。
// 于是点阵折成两行、卡片比邻居高出一截（229 → 326），而且只有**已上车或已满**
// 的那几张会这样，看上去像随机的排版错乱。把徽章移走后只需 271px，
// 从 1920 到 360 全都放得下（320 那一档另有堆叠规则兜住）。
//
// 【为何不是缩小徽章或让点阵允许折行】徽章要能读，点阵折行则会破坏「一眼估量
// 还剩几个位子」——两者都是治症状。徽章描述的本来就是整辆车的状态（这车满了 /
// 我在车上），放在车头与车号同排才是它的正确位置，顺手解决了宽度问题。
//
// 【为何整块先删再建】上车成功后徽章要从「无」变成「已上车」，满员时要变「已满」。
// 只追加不删除的话，同一张卡上会攒出两个徽章（上车一次留一个）。
function fillBadge(head, it){
  if (!head) { return; }
  var old = head.querySelector('.pill');
  if (old) { head.removeChild(old); }

  var pill = null;
  if (it.full) {
    pill = document.createElement('span');
    pill.className = 'pill warn';
    pill.textContent = '已满';
  } else if (it.aboard) {
    pill = document.createElement('span');
    pill.className = 'pill ok';
    pill.textContent = '已上车';
  }
  if (!pill) { return; }

  // 插在发车时间**之前**：.car-when 用 margin-left:auto 把自己顶到最右，
  // 直接 append 会让徽章排到时间的右边、贴在卡片边缘，与车号离得最远。
  var when = head.querySelector('.car-when');
  if (when) { head.insertBefore(pill, when); } else { head.appendChild(pill); }
}

// 卡片底排：左边车费，右边主操作（已上车 → 复制；未上车 → 上车按钮；满员 → 禁用）。
//
// 【为何车费和按钮同排】用户在这一步要做的判断只有一个：「这个价，上不上」。
// 价格和按钮分开摆（旧表格里隔着两列）会让他来回看两处才凑出这个判断。
//
// 【为何整块清空重建，而不是改按钮文字和 class】改文字得把旧的 click 监听器摘掉，
// 而 removeEventListener 需要持有原函数引用；用 cloneNode 绕过又会让调用方
// 持着已脱离文档的旧引用。清空重建只有一条路径，不会出现「按钮写着复制、
// 点下去却又上车一次」这种残留状态。
function fillFoot(foot, it, tktBox, seatsBox, cardEl){
  foot.textContent = '';

  // ---- 车费 ----
  //
  // 【为何两种时态共用一处】用户真正问的是「这辆车对我来说多少钱」。未上车时
  // 答案是「要付多少」，已上车时是「付了多少」——同一个问题的两个时态。
  //
  // 已上车显示的 paid 是**退款后的净支出**：后面每来一个人，这个数字会自己变小。
  // 括号里那句话点明这一点，否则用户会以为自己付得比后来的人贵。
  if (it.keyKind !== 'oauth') {
    var fee = document.createElement('span');
    fee.className = 'car-fee';
    if (it.aboard) {
      fee.appendChild(document.createTextNode('车费 '));
      var pb = document.createElement('b');
      pb.textContent = num(it.paid) + ' 分';
      fee.appendChild(pb);
      fee.appendChild(document.createTextNode('（已付，之后有人上车会退差额）'));
    } else {
      fee.appendChild(document.createTextNode('车费 '));
      var nb = document.createElement('b');
      nb.textContent = num(it.boardPrice) + ' 分';
      fee.appendChild(nb);
    }
    foot.appendChild(fee);
  }

  // ---- 主操作 ----
  if (it.key) {
    var c = document.createElement('button');
    c.className = 'copy';
    c.textContent = '复制车票';
    c.addEventListener('click', function(){ copy(it.key, c); });
    foot.appendChild(c);
    return;
  }

  // OAuth 号没有可买的车票，不给上车按钮——给了就是引导用户为拿不到的东西付费。
  if (it.keyKind === 'oauth') { return; }

  // 积分未启用时不存在「上车」概念，此处什么都不显示（明文已由服务端直接下发）。
  if (it.keyKind !== 'locked') { return; }

  // 只读角色不画按钮。服务端会 403，画出来只是让人点一次、失败一次。
  if (!CAN_BOARD) {
    var ro = document.createElement('span');
    ro.className = 'sub';
    ro.textContent = '只读账号';
    foot.appendChild(ro);
    return;
  }

  var b = document.createElement('button');
  if (it.full) {
    b.className = 'board full';
    b.textContent = '已满';
    b.disabled = true;
    foot.appendChild(b);
    return;
  }
  b.className = 'board';
  b.textContent = '上车 · ' + num(it.boardPrice) + ' 分';
  b.addEventListener('click', function(){ board(it, b, tktBox, seatsBox, foot, cardEl); });
  foot.appendChild(b);
}

// 汇总条。
//
// 【「可复制」为何不直接用 s.copyable】那是服务端在**这次请求时**算的数。上车成功后
// 页面是就地更新的（不整表重刷），服务端的那个数就停在了上车之前——用户会看到
// 第一行明明已显示明文，汇总条却写着「可复制 0」。所以这一格实时数当前行的状态，
// 其余几格与上车无关，仍用服务端值。
function renderSummary(s){
  var box = $('summary');
  box.textContent = '';
  if (!s) return;
  // 只留与「上车」直接相关的三个数。
  //
  // 【为何砍掉 RPM / 在途 / credits / 窗口内请求】那几格与表格里刚删掉的运维列
  // 是同一类东西：用户对它们既无从判断也无从行动，摆在余额条下面只是把
  //「我有几张票、还有几辆能上」挤到看不见。服务端仍照常下发（凭据管理页要用）。
  var copyable = 0;
  var boardable = 0;
  LAST_ITEMS.forEach(function(x){
    if (x.key) { copyable++; }
    // 能上车 = 有票可拿（非 OAuth）、我还没上、且没满。
    if (x.keyKind !== 'oauth' && !x.aboard && !x.full) { boardable++; }
  });
  var cells = [
    ['车队总数', num(s.total)],
    ['我的车票', num(copyable)],
    ['还能上车', num(boardable)]
  ];
  cells.forEach(function(c){
    var d = document.createElement('div');
    d.className = 'sm-item';
    var v = document.createElement('div');
    v.className = 'sm-val';
    v.textContent = c[1];
    var l = document.createElement('div');
    l.className = 'sm-label';
    l.textContent = c[0];
    d.appendChild(v);
    d.appendChild(l);
    box.appendChild(d);
  });
}

// 上车。成功后**就地**把该行的车票换成明文，不整表重刷。
//
// 【为何不直接 loadKeys() 重刷整表】重刷会丢掉滚动位置和其它行的展开状态，
// 而用户此刻的注意力全在刚点的那一行上。就地更新让「点一下，车票出现」这件事
// 是连续的；整表闪一下反而让人怀疑是不是没成功。
function board(it, btn, tktBox, seatsBox, foot, cardEl){
  btn.disabled = true;
  var was = btn.textContent;
  btn.textContent = '上车中…';
  clearSay($('m-list'));
  // 上车在途期间禁止自动刷新整表。
  //
  // 【为何必须挡】轮询若正好在这中间落地，会用服务端的旧数据重建整个列表——
  // 用户看到的是「上车中…」的按钮突然变回「上车 · 10分」，而扣分其实已经成功。
  // 那种画面会让人以为失败了并再点一次（第二次是幂等的，但白白吓一跳）。
  BUSY = true;

  api('board/' + encodeURIComponent(it.id), { method: 'POST' }).then(function(r){
    BUSY = false;
    if (r.status === 401) { show('auth'); return; }

    if (r.status === 402) {
      var b = r.body || {};
      say($('m-list'), '积分不足：需 ' + num(b.needed) + ' 分，当前 ' + num(b.balance) + ' 分');
      btn.disabled = false;
      btn.textContent = was;
      return;
    }
    if (r.status === 409) {
      var f = r.body || {};
      say($('m-list'), '该车已满（' + num(f.count) + '/' + num(f.max) + '）');
      // 满员是终态，按钮不恢复可点——恢复了只会让人再点一次再失败一次。
      btn.textContent = '已满';
      // 座位区整块重画，而不是改一处文本。
      //
      // 【为何不能写 seatsBox.textContent = '2/15'】座位区是标签 + 点阵 + 数字
      // 三个节点，赋 textContent 会把点阵一起抹掉、只剩一行裸数字——
      // 而这正是「表格改卡片」时最容易漏的一处：旧代码那样写是对的（那里只有数字）。
      it.boardCount = f.count;
      it.full = true;
      if (seatsBox) { fillSeats(seatsBox, it); }
      // 徽章在车头（不在座位行，见 fillBadge 的说明），从卡片里取那一行来更新。
      // 漏掉这一步：卡片整张压暗了、按钮写着「已满」，唯独车头没有「已满」徽章，
      // 而刷新一下它又会出现——这种「只在点击后错、刷新就好」的差异最难查。
      if (cardEl) { fillBadge(cardEl.querySelector('.car-h'), it); }
      if (cardEl) { cardEl.className = 'car full'; }
      return;
    }
    if (!r.ok) {
      say($('m-list'), (r.body && r.body.error) || '上车失败');
      btn.disabled = false;
      btn.textContent = was;
      return;
    }

    var d = r.body || {};
    // 把服务端下发的明文写回这一行。d.key 可能为 null（OAuth 号 / 号刚被删），
    // 那时 fillTicket 会按 keyKind 显示对应说明，不会显示空白。
    it.key = d.key || null;
    it.keyKind = d.key ? 'plain' : it.keyKind;
    it.aboard = true;
    it.paid = d.price;
    it.boardCount = d.count;
    fillTicket(tktBox, it);
    // 座位区整块重画：点阵里要多亮一个点，而且那个点是绿的（我的位子）。
    // 同样不能赋 textContent——见上面 409 分支的说明。
    if (seatsBox) { fillSeats(seatsBox, it); }
    // 卡片边框转绿，并立即把「已上车」徽章画到车头。若只在 render() 调
    // fillBadge，上车成功后必须等下一次整表刷新才看得到徽章。
    if (cardEl) {
      cardEl.className = 'car mine';
      fillBadge(cardEl.querySelector('.car-h'), it);
    }

    // 重建整个底排。
    //
    // 【为何整块重建而不改按钮的 class 和文字】那样得把旧的 click 监听器摘掉，
    // 而 removeEventListener 需要持有原函数引用；用 cloneNode/replaceWith 绕过
    // 又会让后续代码持着已脱离文档的旧引用。清空重建只有一条路径，不存在
    // 「按钮看起来是复制、点下去却又上车一次」这种状态残留。
    //
    // 底排同时承载车费文案：上车后要从「车费 10 分」变成「车费 10 分（已付…）」，
    // 重建正好一并解决，不必单独去找那个文本节点。
    fillFoot(foot, it, tktBox, seatsBox, cardEl);

    var msg = '上车成功，扣 ' + num(d.price) + ' 分';
    if (d.already) { msg = '你已经在车上了'; }
    else if (d.refunded > 0) { msg += '，前面 ' + num(d.count - 1) + ' 位共退 ' + num(d.refunded) + ' 分'; }
    say($('m-list'), msg, 'ok');

    // 余额条立刻按响应里的余额更新：不更新的话用户看到扣了分而余额没动，
    // 会以为没成功。用服务端返回的 d.balance 而不是自己减，避免有退款时算错。
    //
    // spent/topup 这次拿不到（上车响应里没有），所以先沿用上一次的值再异步补齐。
    // 不这样做的话它们会闪一下变成 0——看起来像账目被清空了。
    WALLET.balance = d.balance;
    renderWallet(WALLET, true, countAboard());
    // 汇总条的「可复制」也得跟着变：这一行刚拿到明文，可复制数就多了一张。
    // 漏掉这一步会出现「第一行明明显示着明文，汇总条却写可复制 0」。
    renderSummary(LAST_SUMMARY);
    // 价格表上的高亮跟着人数走。board() 上面已就地改了 it.boardCount，
    // 这里用 LAST_ITEMS 重画一次，高亮才会从第 N 格移到第 N+1 格。
    // 漏掉的话：刚上车的人看到人数变了、车费变了，唯独规则表还指着上一格。
    renderRules(LAST_PRICING, true, LAST_ITEMS);
    api('wallet').then(function(wr){
      if (wr.ok && wr.body) {
        WALLET = wr.body;
        renderWallet(WALLET, true, countAboard());
      }
    });
  }).catch(function(err){
    // 【为何 catch 里也要放开 BUSY】漏掉这一句的后果是「上车时网络抖了一下，
    // 从此自动刷新永久停摆」——而页面看起来完全正常，只是数字再也不动了。
    BUSY = false;
    console.error('上车异常:', err);
    btn.disabled = false;
    btn.textContent = was;
    var isNet = err instanceof TypeError && /fetch|network|Failed/i.test(String(err && err.message));
    say($('m-list'), isNet ? '网络错误，请重试' : ('出错：' + (err && err.message ? err.message : String(err))));
  });
}

// 最近一次列表的行数据与钱包快照。
//
// 【为何要存这两份状态】上车成功后只更新一行，不整表重刷（见 board() 的说明），
// 但余额条要显示「已上几辆车」——那是全表的聚合。没有这份快照就只能重拉整表，
// 而重拉正是我们想避免的。WALLET 存的是最后一次权威值，board() 先就地改余额
// 让界面立刻响应，随后用 /wallet 的返回覆盖它。
var LAST_ITEMS = [];
var WALLET = { balance: 0, topup: 0, spent: 0 };
// 汇总条的服务端快照。board() 重画汇总条时要原样带回，否则「车队总数」那格
// 会因为拿不到数据而清空。
var LAST_SUMMARY = null;
// 车费规则的服务端快照。上车成功后 board() 要用它重画规则块，让价格表上的
// 高亮跟着新人数移动。
//
// 【为何要存而不是从 DOM 反读】高亮位置由 priceTable 的下标决定，而那张表只
// 存在于服务端下发的对象里；从已渲染的 DOM 反解析人数与价格，等于把数据源
// 变成自己刚画的像素，任何渲染改动都会静默弄坏它。
var LAST_PRICING = null;
// 当前账号能否上车。由 /me 与登录响应下发（服务端算，见 MeResponse.canBoard）。
//
// 【为何默认 true】这个值只影响「要不要画上车按钮」。默认 false 会让页面在
// /me 返回之前把所有按钮藏起来，正常用户看到的是一瞬间的空白操作列；默认 true
// 时只读用户最坏情况是看到按钮闪一下就消失。真正的拦截在服务端（board 的
// can_board 闸门 + gate_plaintext 的角色门），这里只是不去引导注定失败的点击。
var CAN_BOARD = true;
// 当前账号能否看运营看板。默认 false，理由见 enter() 里那段（与 CAN_BOARD 的
// 兜底方向相反是有意的）。
var CAN_MANAGE = false;
function countAboard(){
  var n = 0;
  LAST_ITEMS.forEach(function(x){ if (x.aboard) { n++; } });
  return n;
}

// 渲染列表。**全程 textContent / createElement，绝不 innerHTML**：
// key、email、错误原因都可能含 < 或 &，用 innerHTML 就是把数据当代码执行。
function render(data){
  var box = $('list');
  box.textContent = '';
  var items = data.items || [];

  // 存一份供上车后重算「已上几辆车」「可复制几张票」。存引用而非深拷贝：
  // board() 就地改的正是这些对象，深拷贝会让计数停留在页面刚加载时的旧值。
  //
  // 【必须在 renderSummary / renderWallet 之前赋值】那两个函数都要数 LAST_ITEMS。
  // 早先的写法把 renderSummary 放在了本行之前，于是它数的是**上一次**渲染留下的
  // 数组——首次加载时数组为空、刷新后又永远滞后一拍，而页面看起来完全正常。
  LAST_ITEMS = items;
  if (data.wallet) { WALLET = data.wallet; }
  LAST_SUMMARY = data.summary || null;
  // 规则快照存下来：board() 就地更新那一行后要重画高亮，而它拿不到 data。
  LAST_PRICING = data.pricing || null;
  renderSummary(LAST_SUMMARY);
  renderWallet(WALLET, data.creditsEnabled, countAboard());
  // 规则说明放在钱包之后、列表之前：先看到「我有多少分」，再看到「怎么收费」，
  // 最后才是「有哪些车」——这是用户第一次进这一页时问题出现的顺序。
  renderRules(LAST_PRICING, data.creditsEnabled, items);
  if (!items.length) {
    var e = document.createElement('div');
    e.className = 'empty';
    e.textContent = '车队里还没有车';
    box.appendChild(e);
    return;
  }

  // 每辆车一张卡。
  //
  // 【为何不再是表格】详见 CSS 里 .cars 那段：这一页没人做跨行的列比较，
  // 而 8 列并排在两头都要付代价（桌面三格折行、手机主操作被推出屏外）。
  //
  // 【这里只展示与「上不上这辆车」有关的东西】RPM / 走势 / 成功率 / Tokens /
  // 延迟 / 最后使用 / 冷却状态是调度和排障才需要的数字，摆进来只会把
  // 「还有几个位子、油还剩多少、多少分」挤到看不见。用户对一把上不了车的 key
  // 也无能为力，显示「冷却中」只增焦虑。这些字段服务端仍照常下发（凭据管理页要用）。
  var grid = document.createElement('div');
  grid.className = 'cars';

  items.forEach(function(it){
    var card = document.createElement('div');

    // 卡片本体的 class 由三种状态决定：自己在车上（绿边）、满员（压暗）、普通。
    // 【为何用整卡边框而不只靠徽章】一屏可能十几张卡，逐张找小徽章费眼；
    // 边框颜色扫一眼就能分。徽章仍保留在座位行——颜色不能是唯一的信息载体。
    card.className = 'car' + (it.aboard ? ' mine' : (it.full ? ' full' : ''));

    // ---- 车头：车号 · 套餐 ·（右）发车时间 ----
    //
    // 【为何第二段要判重】没有备注和邮箱时 displayName 本身就是 `#id`，
    // 无条件再拼一次就会出现「#9001」旁边又是「#9001」——同一个号看两遍。
    // endpoint / region 不进车头：那是路由信息，用户对它无从判断也无从选择。
    var head = document.createElement('div');
    head.className = 'car-h';
    var shown = it.displayName || ('#' + it.id);
    var idEl = document.createElement('span');
    idEl.className = 'car-id';
    idEl.textContent = shown;
    head.appendChild(idEl);
    var bits = [];
    if (shown !== ('#' + it.id)) { bits.push('#' + it.id); }
    if (it.email && it.email !== shown) { bits.push(it.email); }
    if (it.subscriptionTitle) { bits.push(it.subscriptionTitle); }
    if (bits.length) {
      var plan = document.createElement('span');
      plan.className = 'car-plan';
      plan.textContent = bits.join(' · ');
      head.appendChild(plan);
    }

    // 发车时间推到车头最右（.car-when 的 margin-left:auto）。
    //
    // 【为何这里只给相对时间、不给绝对时间】卡片里「这辆车多久前发的」是用来
    // 粗略判断新旧的，精确到分钟的时间戳对这个判断没有增量，却要占掉一行。
    // 绝对时间仍可由 title 悬停看到——需要精确值的人拿得到，不需要的人不被占地方。
    //
    // 【文案必须与 tick() 里那句逐字一致】两处各写一份字符串，改了一处就会出现
    // 「加载时写着 A、一秒后被 tick 改成 B」——本次改名时真踩到过一次。
    if (it.addedAtMs) {
      var when = document.createElement('span');
      when.className = 'car-when age';
      when.setAttribute('data-since', String(it.addedAtMs));
      when.title = '发车时间 ' + ts(it.addedAtMs);
      when.textContent = '已发车 ' + ago(it.addedAtMs);
      head.appendChild(when);
    }
    // 状态徽章跟车号同排（见 fillBadge：它从座位行搬上来是为了不挤折点阵）。
    // 必须在 .car-when 已经挂进 head 之后调用——fillBadge 要靠它决定插入位置。
    fillBadge(head, it);
    card.appendChild(head);

    // ---- 座位：点阵 + 数字 ----
    // 与上车成功后的就地更新**共用** fillSeats，避免两处各写一份「什么时候
    // 画几个点」——那样上车后的样子和刷新后的样子会慢慢长歪。
    var seatsBox = document.createElement('div');
    seatsBox.className = 'car-row';
    fillSeats(seatsBox, it);
    card.appendChild(seatsBox);

    // ---- 油量：上游额度。真实 getUsageLimits 快照，与车队积分完全分开 ----
    var fuelBox = document.createElement('div');
    fillFuel(fuelBox, it);
    card.appendChild(fuelBox);

    // ---- 车票 + 底排：卡片下半部分，用一条细线与上面隔开 ----
    //
    // 【为何车票和车费/按钮在同一块里】上半是「这辆车怎么样」（座位、油量），
    // 下半是「我和这辆车的关系」（我的票、我付的钱、我要不要上）。
    var tkt = document.createElement('div');
    tkt.className = 'car-tkt';

    var tktBox = document.createElement('div');
    fillTicket(tktBox, it);
    tkt.appendChild(tktBox);

    // 我的上车时间。只有自己上过的车才有，且只放在已上车的卡片里。
    //
    // 【为何不显示别人的上车时间】那会暴露「谁在什么时候拿了这把 key」的活动
    // 轨迹，而同一把 key 的乘客之间并不互相认识，也没有理由知道对方的作息。
    if (it.boardedAtMs) {
      var mine = document.createElement('div');
      mine.className = 'car-mine';
      mine.textContent = '你在 ' + ts(it.boardedAtMs) + ' 上车';
      tkt.appendChild(mine);
    }

    var foot = document.createElement('div');
    foot.className = 'car-foot';
    fillFoot(foot, it, tktBox, seatsBox, card);
    tkt.appendChild(foot);

    card.appendChild(tkt);
    grid.appendChild(card);
  });
  box.appendChild(grid);
}

// ==================== 运营看板 ====================
//
// 【为何看板是独立一屏、独立请求、独立定时器】它的数据是全站聚合（一次 SQL 扫
// 全表），成本远高于列表页的一次分页查询。挂在列表页里跟着 10 秒轮询走，等于让
// 每个开着页面的管理员每分钟给数据库来 6 次全表聚合。看板只在打开时拉一次，
// 要新数据自己点刷新。

// 一行「标签 + 数字」的小块，与汇总条同一套排版。
function smItem(label, value){
  var d = document.createElement('div');
  d.className = 'sm-item';
  var v = document.createElement('div');
  v.className = 'sm-val';
  v.textContent = value;
  var l = document.createElement('div');
  l.className = 'sm-label';
  l.textContent = label;
  d.appendChild(v);
  d.appendChild(l);
  return d;
}

// 区块骨架：清空 + 标题，返回给调用方往里填内容。
function blk(id, title){
  var box = $(id);
  box.textContent = '';
  var h = document.createElement('div');
  h.className = 'blk-h';
  h.textContent = title;
  box.appendChild(h);
  return box;
}

// 一横排数字。
function renderWindow(id, title, w){
  var box = blk(id, title);
  var row = document.createElement('div');
  row.className = 'sm-row';
  var d = w || {};
  [
    ['发车票', num(d.tickets)],
    ['充值', num(d.topup) + ' 分'],
    ['消费', num(d.spend) + ' 分'],
    ['退款', num(d.refund) + ' 分'],
    // 调账可正可负，负号必须显式带出来——去掉符号会让「扣了 50 分」看起来像加了 50。
    ['管理员调账', (d.adjust > 0 ? '+' : '') + num(d.adjust) + ' 分']
  ].forEach(function(c){ row.appendChild(smItem(c[0], c[1])); });
  box.appendChild(row);
}

// 用户分层。
function renderTiers(t){
  var box = blk('dash-tiers', '用户分层');
  var row = document.createElement('div');
  row.className = 'sm-row';
  var d = t || {};
  [
    ['总用户', num(d.total)],
    ['活跃（上过车有余额）', num(d.active)],
    ['欠费（上过车没余额）', num(d.broke)],
    ['僵尸（从未上车）', num(d.zombie)],
    ['已停用', num(d.disabled)]
  ].forEach(function(c){ row.appendChild(smItem(c[0], c[1])); });
  box.appendChild(row);

  // 停用与前三档是交叉关系，不写清楚会让人把五个数加起来发现超过总数而怀疑数据。
  var note = document.createElement('div');
  note.className = 'sub';
  note.textContent = '活跃 + 欠费 + 僵尸 = 总用户；已停用与这三档交叉统计，不参与相加。';
  box.appendChild(note);
}

// 车辆热度榜。
function renderKeys(keys){
  var box = blk('dash-keys', '车辆热度（按乘客数）');
  var rows = keys || [];
  if (!rows.length) {
    var e = document.createElement('div');
    e.className = 'empty';
    e.textContent = '还没有人上车';
    box.appendChild(e);
    return;
  }
  var table = document.createElement('table');
  var thead = document.createElement('thead');
  var htr = document.createElement('tr');
  [['凭据', false], ['乘客', false], ['当前单价', false], ['在册收入', false],
   ['首次上车', true], ['最近上车', true]].forEach(function(c){
    var th = document.createElement('th');
    th.textContent = c[0];
    if (c[1]) { th.className = 'hide-sm'; }
    htr.appendChild(th);
  });
  thead.appendChild(htr);
  table.appendChild(thead);
  var tbody = document.createElement('tbody');
  rows.forEach(function(k){
    var tr = document.createElement('tr');
    var tdId = document.createElement('td');
    // 【为何这里不用 num()】num() 加千分位，那是给「多少分」「多少人」这类**数量**
    // 用的。凭据 id 是个标识符，`#9,001` 会被读成「9001 个什么」，而且跟凭据管理页
    // 里显示的 `#9001` 对不上——运营要靠这个号去那边查，两边写法必须一致。
    // 实测截图就是这么发现的。
    tdId.textContent = '#' + String(k.credentialId);
    tr.appendChild(tdId);
    var tdP = document.createElement('td');
    tdP.className = 'num';
    tdP.textContent = num(k.passengers);
    tr.appendChild(tdP);
    var tdU = document.createElement('td');
    tdU.className = 'num';
    // unitPrice 缺失时服务端整个字段不下发（有乘客必有价格快照，缺了说明库被外部
    // 动过）。显示「—」而不是 0：0 分意味着免费，那是个具体且错误的断言。
    tdU.textContent = (k.unitPrice === undefined || k.unitPrice === null)
      ? '—' : (num(k.unitPrice) + ' 分');
    tr.appendChild(tdU);
    var tdR = document.createElement('td');
    tdR.className = 'num';
    tdR.textContent = num(k.revenue) + ' 分';
    tr.appendChild(tdR);
    var tdF = document.createElement('td');
    tdF.className = 'hide-sm sub';
    tdF.textContent = ts(k.firstBoardedMs);
    tr.appendChild(tdF);
    var tdL = document.createElement('td');
    tdL.className = 'hide-sm sub';
    tdL.textContent = ts(k.lastBoardedMs);
    tr.appendChild(tdL);
    tbody.appendChild(tr);
  });
  table.appendChild(tbody);
  box.appendChild(table);

  // 在册收入不是历史收款：差额退款会把已付的钱退回去，这个数会自己变小。
  // 不解释的话，管理员拿它跟流水里的充值总额对不上会以为丢了账。
  var note = document.createElement('div');
  note.className = 'sub';
  note.textContent = '在册收入 = 当前所有乘客实付之和。新乘客上车会触发差额退款，'
    + '所以这个数会随人数增加而下降，它不等于历史累计收款。';
  box.appendChild(note);
}

// 登录失败 IP 榜。
function renderFails(rows){
  var box = blk('dash-fails', '近 24 小时登录失败来源');
  var list = rows || [];
  if (!list.length) {
    var e = document.createElement('div');
    e.className = 'empty';
    e.textContent = '近 24 小时没有登录失败';
    box.appendChild(e);
    return;
  }
  var table = document.createElement('table');
  var tbody = document.createElement('tbody');
  list.forEach(function(f){
    var tr = document.createElement('tr');
    var tdIp = document.createElement('td');
    tdIp.className = 'k';
    // 审计里 IP 可能取不到（服务端如实报 null）。显示「未知来源」而不是空白格，
    // 空白格看起来像渲染坏了。
    tdIp.textContent = f.clientIp || '未知来源';
    tr.appendChild(tdIp);
    var tdC = document.createElement('td');
    tdC.className = 'num';
    tdC.textContent = num(f.count) + ' 次';
    tr.appendChild(tdC);
    tbody.appendChild(tr);
  });
  table.appendChild(tbody);
  box.appendChild(table);
}

// 账目自检。
//
// 【为何异常时要把两个数都摆出来】只说「账目异常」等于把人推去翻数据库。
// 余额之和与流水之和一起给，差多少一眼可算，也能立刻判断是偏了一笔还是全错。
function renderIntegrity(c){
  var box = blk('dash-integrity', '账目自检');
  var d = c || {};
  var p = document.createElement('span');
  p.className = d.ok ? 'pill ok' : 'pill bad';
  p.textContent = d.ok ? '账目平衡' : '账目异常';
  box.appendChild(p);

  var row = document.createElement('div');
  row.className = 'sm-row';
  row.style.marginTop = '12px';
  [
    ['余额之和', num(d.balanceSum) + ' 分'],
    ['流水之和', num(d.ledgerSum) + ' 分'],
    ['钱包不一致用户', num(d.walletViolations)]
  ].forEach(function(x){ row.appendChild(smItem(x[0], x[1])); });
  box.appendChild(row);

  if (!d.ok) {
    var why = document.createElement('div');
    why.className = 'sub';
    why.textContent = '余额之和应恒等于流水之和，且每个用户的余额应等于「累计充值 - 累计消费」。'
      + '不成立说明有人绕过流水直接改过余额，或升级过程中丢过流水，需要人工核对。';
    box.appendChild(why);
  }
}

// 拉看板。
//
// 【为何每条失败分支都要 hideBlocks()】六个区块是带边框背景的 .blk，取不到数据时
// 它们是六个**空框**。实测截图：关掉积分开关后整屏是一句错误提示加六个空盒子，
// 看起来像页面塌了，而实际上只是一个开关没开。要么有内容，要么不出现。
function hideBlocks(){ $('dash-body').style.display = 'none'; }

function loadDash(){
  clearSay($('m-dash'));
  return api('admin/dashboard').then(function(r){
    if (r.status === 401) { hideBlocks(); show('auth'); return; }
    // 403 只可能出现在「角色刚被降级、页面还没刷新」这种情况。说清楚原因并把人
    // 送回列表页，比留在一屏空区块上更有用。
    if (r.status === 403) {
      hideBlocks();
      say($('m-dash'), '当前账号没有看板权限');
      return;
    }
    // 404 是**配置**，不是故障：关掉 portalCreditsEnabled 时服务端整条路由都不注册。
    //
    // 【为何非得单独说】笼统报「看板读取失败」会让管理员去查数据库、翻日志，
    // 而真正的原因是他自己（或前一任）把积分开关关了——一句话就能说清的事，
    // 兜底文案会让人查半小时。实测截图正是这样发现的：关掉开关后看板只显示
    // 「看板读取失败」加六个空区块。
    if (r.status === 404) {
      hideBlocks();
      say($('m-dash'), '积分功能未启用（portalCreditsEnabled=false），没有运营数据可看');
      return;
    }
    if (!r.ok) {
      hideBlocks();
      say($('m-dash'), (r.body && r.body.error) || '看板读取失败');
      return;
    }
    renderDash(r.body || {});
  }).catch(function(err){
    hideBlocks();
    console.error('看板请求异常:', err);
    var isNet = err instanceof TypeError && /fetch|network|Failed/i.test(String(err && err.message));
    say($('m-dash'), isNet ? '网络错误，请重试' : ('页面出错：' + (err && err.message ? err.message : String(err))));
  });
}

function renderDash(d){
  // 【为何这里要显式打开】上一次拉取失败过的话区块是隐藏的，只填内容不改
  // display 会让重试成功后整屏仍然空白——那比第一次就失败更难理解，因为错误
  // 提示已经被清掉了，看起来像「点了刷新什么都没发生」。
  $('dash-body').style.display = 'block';
  // 窗口起点照实显示。不显示的话「今日」到底从几点算起只能靠猜——服务端按
  // 本地时区的零点算，而看板可能被另一个时区的人打开。
  $('dash-when').textContent = d.sinceMs ? ('· 今日自 ' + ts(d.sinceMs) + ' 起') : '';
  renderIntegrity(d.integrity);
  renderWindow('dash-today', '今日', d.today);
  renderWindow('dash-total', '累计', d.total);
  renderTiers(d.tiers);
  renderKeys(d.keys);
  renderFails(d.loginFails);
}

// ==================== 审计 ====================
//
// 【为何审计独立第四屏，而不是看板上的第七个区块】审计是**逐条翻阅**的东西：
// 有筛选、有分页、一次几十行。塞进看板会让那六块聚合数字被推到一屏之外，
// 而两者的使用场景不同——看板是「每天看一眼」，审计是「出事了来查」。

// 当前分页状态。
//
// 【为何 offset 存在这里而不是每次从 DOM 读】翻页时要「保持筛选条件不变、只改
// offset」。从输入框重读筛选是对的（用户可能改了框但没点筛选，那时按新条件翻页
// 反而符合预期？——不对：那会让「下一页」跳到另一批数据上，用户以为翻页坏了）。
// 所以筛选条件在点「筛选」时被**冻结**进 AUDIT_Q，翻页只动 offset。
var AUDIT_Q = null;
var AUDIT_OFFSET = 0;
var AUDIT_TOTAL = 0;
// 最近一次审计响应的原样快照。
//
// 【为何必须存这一份】翻页要知道「上一次真实请求用的 limit 是多少」，而那个值
// 只有服务端回显里有（它可能把 limit 钳小了）。第一版忘了声明这个变量，两个翻页
// handler 直接引用它 —— 点「下一页」抛 ReferenceError、翻页彻底失效，而这个异常
// 发生在 handler 内部，页面其它部分照常工作，控制台之外没有任何迹象。
// 实测截图才发现（分页条一直显示「1–50」，点了没反应）。
var LAST_AUDIT = null;

// datetime-local 的值 → 毫秒。空串给 null（表示不筛）。
//
// 【为何用 new Date(str) 而不是手工拆分】`datetime-local` 给的是无时区的
// `2026-08-06T19:30`，new Date 会按**浏览器本地时区**解析——这正是用户的意图：
// 他填的是自己看到的时间。手工拆再拼 UTC 会让筛选窗口整体偏移几小时，
// 而偏移的表现是「明明有记录却筛不出来」，最难查。
function localToMs(v){
  if (!v) { return null; }
  var t = new Date(v).getTime();
  return isNaN(t) ? null : t;
}

// 结束时间专用：把用户选的那一刻**补到它所在单位的末尾**。
//
// 【为何必须补】`<input type="datetime-local">` 不带 step 时粒度是分钟，选「22:00」
// 拿到的是 22:00:00.000。而服务端的上界是闭区间（`at_ms <= ?`），于是 22:00:30
// 那条记录被排除——用户以为自己筛的是「到 22:00 这一分钟为止」，实际只到那一分钟的
// 第 0 毫秒。最多静默漏掉 59.999 秒的数据，而且**没有任何迹象**：界面上就是少几条，
// 看起来像那段时间真的没事发生。审计里「少几条」和「没发生」是完全不同的结论。
//
// 【为何按字符串长度判断粒度而不是写死 59999】将来给输入框加上 step="1" 就会带上
// 秒（长度 19），那时补 59999 会把窗口多撑将近一分钟——又是一个反方向的静默偏差。
// 按实际给了几位来补，两种粒度都对。
function localToMsEnd(v){
  var t = localToMs(v);
  if (t === null) { return null; }
  // 'YYYY-MM-DDTHH:MM' = 16 字符（无秒）→ 补到 59.999 秒
  // 'YYYY-MM-DDTHH:MM:SS' = 19 字符（有秒）→ 只补到 .999 毫秒
  return v.length <= 16 ? t + 59999 : t + 999;
}

// 毫秒 → datetime-local 需要的 `YYYY-MM-DDTHH:MM`（本地时区）。
// 【这里原先有个 msToLocal（毫秒 → datetime-local 的值格式）】
// 它是为「把当前筛选回填进时间框」写的，但那个需求不存在：两个时间框由用户填，
// 页面从不反向写入它们。留着一个没人调用的时间格式化函数，下一个人会以为
// 页面有「回填筛选条件」这个能力，从而按那个前提改代码。
// no_dead_js_functions 抓到了它。

// 从筛选框读出一份查询条件。
//
// 【为何动作下拉的空值就是「不筛」】下拉第一项是「全部动作」，value 为空串。
// 服务端把空串当 None（见 AuditParams::to_query 的 clean），所以这里不必特殊处理——
// 但**必须两边一致**：若服务端改成「精确匹配空串」，这里会静默变成 0 条。
// http.rs 那侧的 clean() 有注释锁住这个约定。
function readFilters(){
  return {
    username: $('f-user').value.trim(),
    action: $('f-action').value,
    // 动作族前缀。与 action 是**两个独立字段**：下拉选精确值，这里填前缀。
    // 服务端两个都支持且可同时生效（AND），所以不必在前端做互斥。
    actionPrefix: $('f-prefix').value.trim(),
    sinceMs: localToMs($('f-since').value),
    // 结束时间走补末尾的那个函数（理由见 localToMsEnd）。
    untilMs: localToMsEnd($('f-until').value),
    limit: parseInt($('f-page').value, 10) || 50
  };
}

// 把查询条件拼成 URL 查询串。导出与列表共用它——两处各拼一遍的话，
// 导出的 CSV 与屏幕上看到的会是两批数据（服务端已有测试锁这件事，
// 但前端这一层同样能把它搞错）。
function auditQs(q, offset){
  var parts = [];
  if (q.username) { parts.push('username=' + encodeURIComponent(q.username)); }
  if (q.action) { parts.push('action=' + encodeURIComponent(q.action)); }
  if (q.actionPrefix) { parts.push('actionPrefix=' + encodeURIComponent(q.actionPrefix)); }
  if (q.sinceMs) { parts.push('sinceMs=' + q.sinceMs); }
  if (q.untilMs) { parts.push('untilMs=' + q.untilMs); }
  if (offset) { parts.push('offset=' + offset); }
  if (q.limit) { parts.push('limit=' + q.limit); }
  return parts.length ? ('?' + parts.join('&')) : '';
}

// 导出链接跟着当前筛选走。
//
// 【为何不带 offset】导出的语义是「把我筛出来的这批全拿走」，不是「拿走当前这页」。
// 带上 offset 会让人导出第 3 页后发现文件里少了前两页的内容，而他以为导的是全部。
function syncExportHref(){
  var q = AUDIT_Q || readFilters();
  // limit 不带：服务端对导出用自己的上限（AUDIT_EXPORT_MAX），
  // 把页面的「每页 50」带过去会让导出也只有 50 条。
  var forExport = {
    username: q.username, action: q.action, actionPrefix: q.actionPrefix,
    sinceMs: q.sinceMs, untilMs: q.untilMs, limit: null
  };
  $('audit-export').setAttribute('href', '/portal/api/admin/audit.csv' + auditQs(forExport, 0));
}

// 填充动作下拉。
function renderActionOptions(rows){
  var sel = $('f-action');
  var keep = sel.value;
  sel.textContent = '';
  var all = document.createElement('option');
  all.value = '';
  all.textContent = '全部动作';
  sel.appendChild(all);
  (rows || []).forEach(function(a){
    var o = document.createElement('option');
    o.value = a.action;
    o.textContent = a.action + '（' + num(a.count) + '）';
    sel.appendChild(o);
  });
  // 刷新后保住用户已选的那一项：不保的话每次刷新都跳回「全部动作」，
  // 而用户往往是在盯着某一类动作反复刷新。
  if (keep) { sel.value = keep; }
}

// 审计表格。
//
// 【为何 detail 也用 textContent】detail 里有管理员自填的备注和用户名，
// 都可能含 `<`。这一页的红线是全程 textContent（见文件头），审计这张表
// 恰恰是最可能出现攻击者可控字符串的地方。
function renderAudit(page){
  var box = $('audit-list');
  box.textContent = '';
  var rows = (page && page.rows) || [];
  AUDIT_TOTAL = (page && page.total) || 0;

  if (!rows.length) {
    var e = document.createElement('div');
    e.className = 'empty';
    // 分空库与「筛没了」两种情况说话：前者是「还没有数据」，后者是「条件太窄」。
    // 混成一句「没有数据」会让人以为审计没在记录。
    e.textContent = AUDIT_TOTAL === 0 && !hasAnyFilter()
      ? '还没有审计记录'
      : '当前筛选条件下没有记录';
    box.appendChild(e);
  } else {
    var table = document.createElement('table');
    var thead = document.createElement('thead');
    var htr = document.createElement('tr');
    [['时间', false], ['用户', false], ['动作', false], ['来源 IP', true], ['详情', true]]
      .forEach(function(c){
        var th = document.createElement('th');
        th.textContent = c[0];
        if (c[1]) { th.className = 'hide-sm'; }
        htr.appendChild(th);
      });
    thead.appendChild(htr);
    table.appendChild(thead);

    var tbody = document.createElement('tbody');
    rows.forEach(function(r){
      var tr = document.createElement('tr');

      var tdT = document.createElement('td');
      tdT.className = 'sub';
      tdT.textContent = ts(r.atMs);
      tr.appendChild(tdT);

      var tdU = document.createElement('td');
      // 用户名可能为空（未知用户的登录失败就没有用户名）。显示占位而不是空格。
      tdU.textContent = r.username || '—';
      tr.appendChild(tdU);

      // 动作上色：失败类标红、成功类标绿、管理员操作标黄。
      //
      // 【为何按前缀判而不是列一份完整映射表】新增一种失败原因（服务端只是多一个
      // 字符串）不该要求同步改前端映射表——漏改的表现是新动作显示成普通灰色，
      // 于是一批失败记录在视觉上消失在成功记录里。前缀判定让新动作自动归类。
      var tdA = document.createElement('td');
      var pill = document.createElement('span');
      pill.className = 'pill mono ' + actionClass(r.action);
      pill.textContent = r.action;
      tdA.appendChild(pill);
      tr.appendChild(tdA);

      var tdI = document.createElement('td');
      tdI.className = 'hide-sm k';
      tdI.textContent = r.clientIp || '—';
      tr.appendChild(tdI);

      var tdD = document.createElement('td');
      tdD.className = 'hide-sm sub detail';
      tdD.textContent = r.detail || '';
      tr.appendChild(tdD);

      tbody.appendChild(tr);
    });
    table.appendChild(tbody);
    box.appendChild(table);
  }

  // 存一份服务端回显，翻页要用它的 limit / hasMore（见 LAST_AUDIT 的说明）。
  LAST_AUDIT = page || null;
  renderPager(page);
  $('audit-count').textContent = AUDIT_TOTAL
    ? ('· 共 ' + num(AUDIT_TOTAL) + ' 条')
    : '';
}

// 动作 → 徽章配色。前缀判定，新动作自动归类（理由见调用处）。
function actionClass(a){
  if (!a) { return 'warn'; }
  if (a.indexOf('_fail') >= 0) { return 'bad'; }
  if (a.indexOf('admin_') === 0) { return 'warn'; }
  if (a.indexOf('_ok') >= 0) { return 'ok'; }
  return 'warn';
}

// 当前是否有任何筛选条件（用于区分「空库」和「筛没了」）。
function hasAnyFilter(){
  var q = AUDIT_Q || readFilters();
  // actionPrefix 必须算进来：漏掉它的话，只按动作族筛而筛空时，
  // 空状态会说「还没有审计记录」——而库里可能有几千条，只是这个族没有。
  return !!(q.username || q.action || q.actionPrefix || q.sinceMs || q.untilMs);
}

// 翻页条。
//
// 【为何「上一页」按 offset 判、「下一页」按服务端的 hasMore 判】末页的判断
// 若在前端算（offset+len<total），写错一次就会留一个点不动的「下一页」按钮，
// 而那种错前端测不出来。服务端已经算好了，直接用。
function renderPager(page){
  var box = $('audit-pager');
  if (!page || !AUDIT_TOTAL) { box.style.display = 'none'; return; }
  box.style.display = 'flex';

  // offset/limit 一律用**服务端回显的值**，不用本地那份。
  //
  // 【为何这很重要】服务端会把 limit 钳到上限（AUDIT_PAGE_MAX）。用本地值显示的话，
  // 有人把 limit=9999 塞进地址栏时，页面会写「每页 9999」而实际只回了 200 条——
  // 一个说谎的分页条比没有分页条更糟，因为它让人以为自己看到了全部。
  // 同理 offset：服务端把负数钳成 0，本地那份仍是负的，「第 -30 条起」就出来了。
  var limit = page.limit || (AUDIT_Q && AUDIT_Q.limit) || 50;
  var off = page.offset || 0;
  var shown = (page.rows && page.rows.length) || 0;
  // 空页时不写「1–0」这种区间，直接说本页无记录。
  $('p-info').textContent = (shown ? ((off + 1) + '–' + (off + shown)) : '本页无记录')
    + ' / 共 ' + num(AUDIT_TOTAL) + ' 条（每页 ' + limit + '）';
  $('p-prev').disabled = off <= 0;
  // 「还有下一页」由服务端算（见 AuditPage::has_more）：前端自己比较
  // offset+len<total 写错一次就是末页多出一个点不动的按钮。
  $('p-next').disabled = !page.hasMore;
}

// 拉一页审计。`freeze=true` 时把筛选框的当前值冻结进 AUDIT_Q 并回到第一页。
function loadAudit(freeze){
  if (freeze) {
    AUDIT_Q = readFilters();
    AUDIT_OFFSET = 0;
  }
  if (!AUDIT_Q) { AUDIT_Q = readFilters(); }
  clearSay($('m-audit'));
  syncExportHref();

  return api('admin/audit' + auditQs(AUDIT_Q, AUDIT_OFFSET)).then(function(r){
    if (r.status === 401) { show('auth'); return; }
    if (r.status === 403) { say($('m-audit'), '当前账号没有审计权限'); return; }
    if (!r.ok) { say($('m-audit'), (r.body && r.body.error) || '审计读取失败'); return; }
    renderAudit(r.body || {});
  }).catch(function(err){
    console.error('审计请求异常:', err);
    var isNet = err instanceof TypeError && /fetch|network|Failed/i.test(String(err && err.message));
    say($('m-audit'), isNet ? '网络错误，请重试' : ('页面出错：' + (err && err.message ? err.message : String(err))));
  });
}

// 动作下拉的数据。**失败不报错**：下拉填不上只是少一个便利，
// 审计列表本身仍然可用，为此弹一条红字反而像整页坏了。
function loadAuditActions(){
  return api('admin/audit/actions').then(function(r){
    if (r.ok) { renderActionOptions(r.body || []); }
  }).catch(function(err){
    console.error('动作列表读取失败（不影响审计列表）:', err);
  });
}

// 进审计屏。
function enterAudit(){
  show('audit');
  loadAuditActions();
  loadAudit(true);
}

// silent=true 是自动轮询用的：不清空已有提示，也不因为一次网络抖动就弹错误。
//
// 【为何要分这两种模式】手动点「刷新」失败必须说出来，否则用户不知道自己看的是
// 旧数据。但轮询每 10 秒一次，一次失败就报错会让偶发抖动变成满屏红字，而且会
// 把「积分不足」这类用户正在读的提示冲掉。轮询失败就静默跳过，下一轮自然补上。
function loadKeys(silent){
  if (!silent) { clearSay($('m-list')); }
  return api('keys').then(function(r){
    if (r.status === 401) { show('auth'); return; }
    if (!r.ok) {
      if (!silent) { say($('m-list'), (r.body && r.body.error) || '读取失败'); }
      return;
    }
    render(r.body);
  }).catch(function(err){
    if (silent) { console.error('自动刷新失败（已忽略，下一轮重试）:', err); return; }
    // 【为何要分辨异常来源】这里原先无条件说「网络错误」，结果 render() 里的一个
    // TypeError（少了个 DOM 容器）被伪装成网络问题，排查时完全找错方向。
    // fetch 失败才是网络问题；其它异常是本页 JS 的 bug，如实说出来并打 console。
    console.error('portal 渲染/请求异常:', err);
    var isNet = err instanceof TypeError && /fetch|network|Failed/i.test(String(err && err.message));
    say($('m-list'), isNet ? '网络错误，请重试' : ('页面出错：' + (err && err.message ? err.message : String(err))));
  });
}

// 上车请求在途标志。轮询看到它为真就跳过这一轮（见 board() 里的说明）。
var BUSY = false;
// 单号额度刷新在途数。大于 0 时暂停整表轮询，避免旧快照覆盖刚刷新的单元格。
var QUOTA_BUSY = 0;
// 两个定时器的句柄。enter() 里启动，logout 时清掉——不清的话退出登录后
// 轮询还在打 /keys，每次都 401，控制台刷满错误。
var TICK_T = null;
var POLL_T = null;

// 每秒改写所有「已发车 X」。文案须与 render 里写入时逐字一致，否则加载后一秒变样。
//
// 【为何不整表重渲染】那会让每秒都重建一遍 DOM：正在复制的选区被清掉、
// 鼠标悬停态闪断，纯粹为了改几个字。这里只改文本节点。
//
// 【文案必须与 render 里那处一致】这里和建行处各写一遍前缀，改了一处没改另一处
// 的表现是：首屏显示新文案，一秒后被这里改回旧文案——只在页面停留超过一秒才看得见。
function tick(){
  var nodes = document.querySelectorAll('.age[data-since]');
  for (var i = 0; i < nodes.length; i++) {
    var since = parseInt(nodes[i].getAttribute('data-since'), 10);
    if (since) { nodes[i].textContent = '已发车 ' + ago(since); }
  }
  var quotaNodes = document.querySelectorAll('.quota-age[data-cached-at]');
  for (var j = 0; j < quotaNodes.length; j++) {
    var cachedAt = Number(quotaNodes[j].getAttribute('data-cached-at'));
    if (cachedAt) { quotaNodes[j].textContent = quotaAge(cachedAt); }
  }
}

// 定时重拉整表，让「凭据管理页删掉的号」在这一页也消失。
//
// 【为何是 10 秒】这一页的数据变化来自另一个人的操作（管理员加号/删号、
// 别人上车改变人数与车费）。10 秒足够让人感觉「它自己会更新」，又不至于
// 让一个开着页面的用户每分钟打 60 次请求。
// 【为何 hidden 时不拉】标签页在后台时没人看，继续轮询纯属浪费；切回来
// 时 visibilitychange 会立刻补一次，所以不会看到过期数据。
// 【为何还要判当前是不是列表屏】管理员切到看板后这个定时器仍在跑，而它拉的
// 是列表页的数据、写的是隐藏 DOM——没人看得见，纯粹是白打请求。看板有自己的
// 刷新按钮，不共用这个周期（看板是聚合全表的查询，10 秒一次太重）。
function poll(){
  if (BUSY || QUOTA_BUSY > 0 || document.hidden) { return; }
  if ($('view-list').style.display === 'none') { return; }
  loadKeys(true);
}

// 停掉两个定时器。退出登录、会话过期都要调。
//
// 【为何非停不可】不停的话轮询会继续打 /keys，每 10 秒一个 401，控制台刷满
// 错误；更糟的是 loadKeys 的 401 分支会不断 show('auth')，用户正在重新输密码
// 时输入框被反复重置。
function stopTimers(){
  if (TICK_T) { clearInterval(TICK_T); TICK_T = null; }
  if (POLL_T) { clearInterval(POLL_T); POLL_T = null; }
}

function enter(me){
  $('who').textContent = me.username ? ('· ' + me.username) : '';
  // 角色由服务端算好下发（canBoard），前端不按 role 字符串自己判断——
  // 判断规则只该有一份，多一份就多一处会漂移的地方。
  //
  // 【为何用 !== false 而不是直接赋值】老服务端（或降级回滚）不下发这个字段，
  // 那时 undefined 会让所有人都变成只读，整页按钮消失。缺字段应当按「没有限制」
  // 对待：真正的拦截在服务端，前端这一层只负责不引导注定失败的点击。
  CAN_BOARD = (me.canBoard !== false);
  if (!CAN_BOARD) {
    $('who').textContent += ' · 只读';
  }
  // 看板入口：与 canBoard 相反，缺字段按 false 处理。
  //
  // 【为何两个字段的兜底方向相反】canBoard 缺失说明是老服务端，而老服务端本来
  // 人人可上车，按「没限制」兜底才不会误伤。canManage 缺失同样说明是老服务端，
  // 但老服务端根本没有 /admin/dashboard 这条路由——把按钮画出来，点下去是 404。
  // 兜底方向该由「猜错的后果」决定，不是由「哪个写法顺手」决定。
  CAN_MANAGE = (me.canManage === true);
  // 看板与审计两个入口同进同出：它们受同一道 require_admin 把门，
  // 分开控制只会制造「一个能点一个点不动」的错觉。
  $('to-dash').style.display = CAN_MANAGE ? 'inline-block' : 'none';
  $('to-audit').style.display = CAN_MANAGE ? 'inline-block' : 'none';
  if (CAN_MANAGE) {
    $('who').textContent += ' · 管理员';
  }
  show('list');
  loadKeys();
  // 先停再起：重复登录（退出后再登入）不该叠出第二组定时器，那会让计时器
  // 每秒跳两次、轮询频率翻倍，且只有第二组的句柄被记住、第一组永远关不掉。
  stopTimers();
  TICK_T = setInterval(tick, 1000);
  POLL_T = setInterval(poll, 10000);
}

$('tab-login').addEventListener('click', function(){ setMode('login'); });
$('tab-reg').addEventListener('click', function(){ setMode('register'); });

$('form-auth').addEventListener('submit', function(ev){
  ev.preventDefault();
  var m = $('m-auth');
  clearSay(m);
  var username = $('u').value.trim();
  var password = $('p').value;
  if (!username || !password) { say(m, '请填写用户名和密码'); return; }

  var payload = { username: username, password: password };
  if (mode === 'register') { payload.inviteCode = $('c').value; }

  $('go').disabled = true;
  api(mode, { method: 'POST', body: JSON.stringify(payload) }).then(function(r){
    $('go').disabled = false;
    if (!r.ok) { say(m, (r.body && r.body.error) || '失败，请重试'); return; }
    $('p').value = '';
    if ($('c')) { $('c').value = ''; }
    enter(r.body || {});
  }).catch(function(){
    $('go').disabled = false;
    say(m, '网络错误，请重试');
  });
});

// 【为何要包一层而不是直接传 loadKeys】addEventListener 会把 click 事件对象
// 当第一个实参传进去，而 loadKeys 的第一个形参是 silent —— 一个 Event 是
// truthy，于是手动点「刷新」会走静默分支：读取失败时一声不响，用户点了没反应
// 也看不到任何原因。
$('reload').addEventListener('click', function(){ loadKeys(); });

// 看板进出。
//
// 【为何进看板要每次重拉而不缓存上次结果】这些数字的用途就是「现在什么情况」。
// 显示一份十分钟前的快照而不标注时间，比不显示更糟。
$('to-dash').addEventListener('click', function(){ show('dash'); loadDash(); });
$('dash-reload').addEventListener('click', function(){ loadDash(); });
$('dash-back').addEventListener('click', function(){
  show('list');
  // 回列表页补拉一次：在看板上待着的这段时间轮询是空转的（poll 判了当前屏），
  // 不补拉的话看到的是切走之前的旧表。
  loadKeys(true);
});

// 审计进出。
//
// 【为何进审计要重置到第一页】上次看的可能是第 9 页。带着 offset 进来，而筛选框
// 又是空的（或换了条件），显示的是「全部记录的第 9 页」——用户会以为前面几百条
// 凭空消失了。每次进来都从最新一页开始，这也是审计最常见的看法。
$('to-audit').addEventListener('click', function(){ enterAudit(); });
$('audit-reload').addEventListener('click', function(){ loadAudit(); });
$('audit-to-dash').addEventListener('click', function(){ show('dash'); loadDash(); });
$('audit-back').addEventListener('click', function(){
  show('list');
  loadKeys(true);
});

// 筛选提交。
//
// 【为何一定要 preventDefault】这是个 <form>，不拦的话浏览器会带着查询串
// 整页跳转到 /portal?f-user=…——会话还在，所以页面会重新加载并回到车队列表，
// 看起来像「点了筛选，界面被重置了」。
$('audit-filters').addEventListener('submit', function(ev){
  ev.preventDefault();
  // **必须传 freeze=true**：loadAudit 只在 freeze 时才把筛选框的当前值读进
  // AUDIT_Q，否则用的是上一次冻结的那份（首次进屏是「全空」）。
  //
  // 【实测踩过】这里原先写的是裸 loadAudit()，于是在筛选框里填了用户名、点
  // 「筛选」，请求发出去的仍是没有任何条件的那一版——420 条一条不少地回来了。
  // 页面不报错、按钮有反应、表格确实重绘了一遍，唯独筛选没生效。
  // freeze 同时把 offset 归零，所以这里不必再单独置零。
  loadAudit(true);
});

// 清空必须把**每一个**筛选框都列进来。
//
// 【为何这里最容易漏】新加一个筛选维度时，加输入框、加 readFilters、加 auditQs
// 三处都会想到（不加就没功能），而「清空」不加也照样能用——只是清不掉那一个。
// 表现是：点了清空，四个框空了，结果却还是筛过的，因为 actionPrefix 还在。
// f-clear 的这份清单和 readFilters 读的字段必须一一对应。
$('f-clear').addEventListener('click', function(){
  $('f-user').value = '';
  $('f-action').value = '';
  $('f-prefix').value = '';
  $('f-since').value = '';
  $('f-until').value = '';
  // 【同样必须 freeze】清空输入框只改了 DOM，AUDIT_Q 里仍是上一次冻结的条件。
  // 不 freeze 的话「清空」只会重新拉一遍**旧筛选**的第一页——用户看着四个空框，
  // 屏幕上却还是筛过的结果，而这比筛选没生效更让人困惑（框都空了还在筛）。
  loadAudit(true);
});

// 翻页。
//
// 【为何用 LAST_AUDIT 的 limit 而不是重读下拉框】用户可能在看第 3 页时把每页
// 从 20 改成 100 却没点筛选。此时按下拉框的新值翻页会算出一个跟当前显示无关的
// offset，跳过或重复一批记录。以「上一次真实请求用的 limit」为准，改动在下次
// 点筛选时才生效——那也是 limit 下拉本该有的语义。
$('p-prev').addEventListener('click', function(){
  var lim = (LAST_AUDIT && LAST_AUDIT.limit) || 50;
  AUDIT_OFFSET = Math.max(0, AUDIT_OFFSET - lim);
  loadAudit();
});
$('p-next').addEventListener('click', function(){
  var lim = (LAST_AUDIT && LAST_AUDIT.limit) || 50;
  // 只在服务端说了 hasMore 时才前进。不判的话末页点「下一页」会翻到一个空页，
  // 而按钮此时本该是禁用的——双保险：禁用是表现，这一判是行为。
  if (LAST_AUDIT && LAST_AUDIT.hasMore) {
    AUDIT_OFFSET = AUDIT_OFFSET + lim;
    loadAudit();
  }
});

$('logout').addEventListener('click', function(){
  api('logout', { method: 'POST' }).then(function(){
    stopTimers();
    $('u').value = '';
    $('p').value = '';
    setMode('login');
    show('auth');
  });
});

// 切回前台立刻补一次，不等下一个轮询周期。
//
// 【为何需要这一句】标签页在后台时 poll() 直接 return，且浏览器还会把
// setInterval 节流到分钟级。没有这个补拉，用户切回来第一眼看到的是切走之前
// 的旧数据——而那正是最可能已经变了的时刻（管理员刚删了号）。
document.addEventListener('visibilitychange', function(){
  if (!document.hidden && $('view-list').style.display !== 'none') { poll(); }
});

// 启动：先问一次登录态，决定进哪一屏，避免已登录的人还要再看一次登录框。
api('me').then(function(r){
  if (r.ok && r.body && r.body.username) { enter(r.body); }
  else { show('auth'); }
}).catch(function(){ show('auth'); });
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::PAGE_HTML;
    use std::collections::BTreeSet;

    // ============ 解析辅助 ============
    //
    // 手写扫描而不引 `regex`：这三条检查只需要找几个固定形状的子串，
    // 为此给整个二进制加一个依赖不值得。代价是解析得写细一点，
    // 所以每个测试都自带**对照组**——解析器一旦失效（比如日后页面结构变了、
    // 提取到 0 个符号），断言会在空集合上全部通过，那种"绿"比红更危险。

    /// 取 `<style>…</style>` 之间的内容。
    fn style_block() -> &'static str {
        let start = PAGE_HTML.find("<style>").expect("页面必须有 <style> 块") + "<style>".len();
        let end = PAGE_HTML[start..].find("</style>").expect("<style> 未闭合") + start;
        &PAGE_HTML[start..end]
    }

    /// 取 `<script>…</script>` 之间的内容。
    fn script_block() -> &'static str {
        let start = PAGE_HTML.find("<script>").expect("页面必须有 <script> 块") + "<script>".len();
        let end = PAGE_HTML[start..]
            .find("</script>")
            .expect("<script> 未闭合")
            + start;
        &PAGE_HTML[start..end]
    }

    /// 把 JS 里的注释剔掉。
    ///
    /// 【为何必须剔】页面里有 4 处 `innerHTML` 字样，全部在"绝不用 innerHTML"这类
    /// 说明性注释里。不剔注释的话，`no_inner_html_usage` 会被自己的警示文字绊倒，
    /// 而修法只能是删掉注释——把有价值的说明删掉来让测试变绿，是最坏的结果。
    fn strip_js_comments(src: &str) -> String {
        // 【为何攒 Vec<u8> 而不是 String::push(b as char)】
        // `b as char` 把每个**字节**当成一个字符，中文（UTF-8 三字节）会被拆成三个
        // Latin-1 字符，输出成一串乱码。第一版就是这么写的，症状是断言的失败信息里
        // 出现 "ä¸\u{8a}è½¦" 这种东西——而它同时说明提取结果本身已经不可信了。
        // 按字节找 `//` 和 `/*` 是安全的（UTF-8 续字节恒 >= 0x80，不会等于 ASCII），
        // 所以只要最后按字节序列还原，边界就不会切在字符中间。
        let b = src.as_bytes();
        let mut out: Vec<u8> = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            // 行注释
            if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            // 块注释
            if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(b.len());
                continue;
            }
            out.push(b[i]);
            i += 1;
        }
        String::from_utf8(out).expect("剥注释只删整段 ASCII，不该破坏 UTF-8 边界")
    }

    /// 把 CSS 里的 `/* … */` 注释剔掉。
    ///
    /// 【为何必须剔】本轮实测踩到：删掉 `.quota` 那批类时留了一句墓碑注释
    /// 「【.quota / .quota-main / … 已删】」说明为什么删，结果 `no_dead_css_classes`
    /// 把注释里的类名当成了**定义**，报「定义了但没用：quota, quota-main…」——
    /// 而那几行样式确实已经不在文件里了。
    ///
    /// 修法若是删注释，就变成「为了让测试变绿而删掉解释为什么删的说明」，
    /// 和 `strip_js_comments` 挡的是同一类错误（那边是 innerHTML 的警示文字）。
    ///
    /// 【为何不复用 strip_js_comments】那个还会剥 `//` 行注释，而 CSS 里 `//`
    /// 是合法内容（`url(https://…)`）。这里只认块注释。
    fn strip_css_comments(src: &str) -> String {
        // 按字节扫描的理由同 strip_js_comments：UTF-8 续字节恒 >= 0x80，不会误匹配
        // ASCII 的 `/` 和 `*`，最后按字节还原不会切断中文字符。
        let b = src.as_bytes();
        let mut out: Vec<u8> = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(b.len());
                continue;
            }
            out.push(b[i]);
            i += 1;
        }
        String::from_utf8(out).expect("剥注释只删整段 ASCII，不该破坏 UTF-8 边界")
    }

    /// 找出所有 `$('name')` 里的 name。
    fn js_id_refs() -> BTreeSet<String> {
        let src = strip_js_comments(script_block());
        let mut out = BTreeSet::new();
        let pat = "$('";
        let mut from = 0;
        while let Some(p) = src[from..].find(pat) {
            let s = from + p + pat.len();
            if let Some(q) = src[s..].find('\'') {
                out.insert(src[s..s + q].to_string());
                from = s + q;
            } else {
                break;
            }
        }
        out
    }

    /// 找出**间接**传给 `$()` 的 id：形如 `blk('dash-tiers', …)` 的调用。
    ///
    /// # 为何必须追这一层
    /// [`js_id_refs`] 只认字面量 `$('x')`。看板把区块 id 当参数传进 `blk(id, title)`
    /// 和 `renderWindow(id, …)`，于是 `blk('dash-typo')` 这种错字对那条检查**完全
    /// 隐形**——而它的运行时表现正是那条检查存在的理由：`null.textContent` 抛
    /// TypeError，被 catch 收敛成「页面出错」，看板显示一片空白区块。
    ///
    /// # 为何不硬编码 `blk` / `renderWindow` 两个名字
    /// 那样下一个人抽出第三个这类 helper 时检查会静默失效，而失效的表现就是这个
    /// bug 本身。这里改为**先找出哪些函数会把形参喂给 `$()`**，再回头扫它们的调用
    /// 点取对应位置的字面量。新增 helper 自动进入检查范围。
    ///
    /// # 为何要追到不动点而不是只追一层
    /// 只追一层时 `renderWindow(id, …)` 逃得掉：它自己不调 `$()`，而是把 id 转发给
    /// `blk(id, …)`。实测正是如此——把 `renderWindow('dash-today', …)` 改成一个不存在
    /// 的 id，七条检查全绿。传递闭包让「转发多少层」不再影响检出。
    fn js_id_refs_via_params() -> BTreeSet<String> {
        let src = strip_js_comments(script_block());
        let mut out = BTreeSet::new();

        // 第一遍：把所有 `function name(p0, p1, …) { … }` 的签名与函数体收下来。
        //
        // 函数体按「到下一个顶格 `\nfunction ` 为止」切。本文件所有函数都是顶层
        // 定义、顶格书写，这个切法足够；切歪的后果由下面的对照组兜住。
        struct Fun {
            name: String,
            params: Vec<String>,
            body: String,
        }
        let mut funs: Vec<Fun> = Vec::new();
        let mut from = 0;
        while let Some(p) = src[from..].find("function ") {
            let name_start = from + p + "function ".len();
            let Some(paren) = src[name_start..].find('(') else {
                break;
            };
            let paren = name_start + paren;
            let name = src[name_start..paren].trim().to_string();
            let Some(close) = src[paren..].find(')') else {
                break;
            };
            let close = paren + close;
            let params: Vec<String> = src[paren + 1..close]
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            let body_end = src[close..]
                .find("\nfunction ")
                .map(|i| close + i)
                .unwrap_or(src.len());
            funs.push(Fun {
                name,
                params,
                body: src[close..body_end].to_string(),
            });
            from = close;
        }

        // 第二遍：求不动点。一个形参算「会流向 `$()`」，若它被直接喂给 `$()`，
        // 或被当作实参传进另一个已知会流向 `$()` 的槽位。
        let mut indirect: Vec<(String, usize)> = Vec::new();
        loop {
            let before = indirect.len();
            for f in &funs {
                for (idx, param) in f.params.iter().enumerate() {
                    if indirect.iter().any(|(n, i)| *n == f.name && *i == idx) {
                        continue;
                    }
                    // 直接：`$(param)`
                    let mut flows = f.body.contains(&format!("$({param})"));
                    // 间接：`known(param, …)`，且 param 恰好落在 known 的那个槽位上。
                    if !flows {
                        for (callee, slot) in &indirect {
                            let pat = format!("{callee}(");
                            let mut at = 0;
                            while let Some(p) = f.body[at..].find(&pat) {
                                let open = at + p + pat.len();
                                let prev = f.body[..at + p].chars().next_back();
                                if prev.is_some_and(|c| {
                                    c.is_ascii_alphanumeric() || c == '_' || c == '$'
                                }) {
                                    at = open;
                                    continue;
                                }
                                if let Some(args) = split_args(&f.body[open..]) {
                                    if args.get(*slot).map(|a| a.trim()) == Some(param.as_str()) {
                                        flows = true;
                                        break;
                                    }
                                }
                                at = open;
                            }
                            if flows {
                                break;
                            }
                        }
                    }
                    if flows {
                        indirect.push((f.name.clone(), idx));
                    }
                }
            }
            if indirect.len() == before {
                break;
            }
        }

        // 对照组：直接的 `blk` 和**转发一层**的 `renderWindow` 都必须被认出来。
        // 只断言 blk 的话，传递闭包退化成单层时这条检查照样绿——而那正是实测踩过的洞。
        assert!(
            indirect.iter().any(|(n, i)| n == "blk" && *i == 0),
            "对照组失败：没识别出 blk(id, …) 把形参喂给 $()，解析器失效了。识别到的：{indirect:?}"
        );
        assert!(
            indirect.iter().any(|(n, i)| n == "renderWindow" && *i == 0),
            "对照组失败：没识别出 renderWindow(id, …) 经 blk 转发到 $()，\
             传递闭包退化成单层了。识别到的：{indirect:?}"
        );

        // 第二遍：扫这些函数的调用点，取对应位置的单引号字面量。
        for (name, idx) in &indirect {
            let pat = format!("{name}(");
            let mut from = 0;
            while let Some(p) = src[from..].find(&pat) {
                let open = from + p + pat.len();
                // 前一个字符若是标识符字符，说明命中的是 `xxxblk(` 之类的别的名字。
                let prev = src[..from + p].chars().next_back();
                if prev.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$') {
                    from = open;
                    continue;
                }
                match split_args(&src[open..]) {
                    Some(args) => {
                        if let Some(arg) = args.get(*idx) {
                            let t = arg.trim();
                            // 只收纯字面量实参。变量实参（如 renderWindow(id, …) 内部
                            // 再转发）无法静态判定，跳过而不是猜。
                            if t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'') {
                                out.insert(t[1..t.len() - 1].to_string());
                            }
                        }
                        from = open;
                    }
                    None => break,
                }
            }
        }
        out
    }

    /// 从 `(` 之后的文本里切出顶层实参。遇到不闭合的括号返回 `None`。
    ///
    /// 只跟踪引号与括号深度——足够应付本页的实参形状（字面量、标识符、
    /// 简单成员访问），不追求通用 JS 解析。
    fn split_args(after_open: &str) -> Option<Vec<String>> {
        let mut args = Vec::new();
        let mut cur = String::new();
        let mut depth = 0i32;
        let mut quote: Option<char> = None;
        for c in after_open.chars() {
            if let Some(q) = quote {
                cur.push(c);
                if c == q {
                    quote = None;
                }
                continue;
            }
            match c {
                '\'' | '"' => {
                    quote = Some(c);
                    cur.push(c);
                }
                '(' | '[' | '{' => {
                    depth += 1;
                    cur.push(c);
                }
                ')' if depth == 0 => {
                    args.push(cur);
                    return Some(args);
                }
                ')' | ']' | '}' => {
                    depth -= 1;
                    cur.push(c);
                }
                ',' if depth == 0 => {
                    args.push(std::mem::take(&mut cur));
                }
                _ => cur.push(c),
            }
        }
        None
    }

    /// 找出 HTML 里所有 `id="name"`。
    fn html_ids() -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let pat = "id=\"";
        let mut from = 0;
        while let Some(p) = PAGE_HTML[from..].find(pat) {
            let s = from + p + pat.len();
            if let Some(q) = PAGE_HTML[s..].find('"') {
                out.insert(PAGE_HTML[s..s + q].to_string());
                from = s + q;
            } else {
                break;
            }
        }
        out
    }

    /// 找出所有被**使用**的 CSS 类：JS 的 `className = '…'` 字面量
    /// 加上 HTML 的 `class="…"`。
    ///
    /// 动态拼接（如 `'pill ' + statusClass(x)`）只能取到字面量那一半，
    /// 另一半由 [`css_classes_from_helpers`] 单独补上——那些 helper 返回的
    /// 也是字面量，只是藏在 return 里。
    fn css_classes_used() -> BTreeSet<String> {
        let src = strip_js_comments(script_block());
        let mut out = BTreeSet::new();

        // 逐**行**扫：只要这一行提到 className，就把该行所有单引号字面量当类名收下。
        //
        // 【为何按行而不按 `className = '` 前缀】类名常常不紧跟在赋值号后面：
        //   el.className = 'msg ' + (kind || 'err');
        // 这里的 `err` 是真类名（错误提示的红底靠它），但它在 `||` 后面。
        // 早先我为此单独加了个 `|| '` 模式，结果把 `(r.body && r.body.error) || '读取失败'`
        // 这类默认文案也收成了"类名"，于是六条中文错误提示被报成"用了但未定义的类"。
        // 按行收敛把范围限死在确实与 className 有关的那些行上。
        for line in src.lines() {
            if !line.contains("className") {
                continue;
            }
            // 先去掉 `$('id')`：`$('tab-login').className = …` 里的 id 不是类名，
            // 留着会被误报成"用了但未定义"。
            let cleaned = strip_id_refs(line);
            for lit in single_quoted(&cleaned) {
                for tok in lit.split_whitespace() {
                    if looks_like_class(tok) {
                        out.insert(tok.to_string());
                    }
                }
            }
        }

        // HTML 里的 class="…"
        let mut from = 0;
        while let Some(p) = PAGE_HTML[from..].find("class=\"") {
            let s = from + p + "class=\"".len();
            match PAGE_HTML[s..].find('"') {
                Some(q) => {
                    for tok in PAGE_HTML[s..s + q].split_whitespace() {
                        out.insert(tok.to_string());
                    }
                    from = s + q;
                }
                None => break,
            }
        }
        out
    }

    /// 抹掉 `$('…')`，避免把元素 id 误当成类名。
    fn strip_id_refs(line: &str) -> String {
        let mut out = String::with_capacity(line.len());
        let mut rest = line;
        while let Some(p) = rest.find("$('") {
            out.push_str(&rest[..p]);
            rest = &rest[p + 3..];
            match rest.find('\'') {
                Some(q) => rest = &rest[q + 1..],
                None => break,
            }
        }
        out.push_str(rest);
        out
    }

    /// 取出一行里所有单引号字面量。
    fn single_quoted(line: &str) -> Vec<&str> {
        let mut out = Vec::new();
        let mut rest = line;
        while let Some(p) = rest.find('\'') {
            rest = &rest[p + 1..];
            match rest.find('\'') {
                Some(q) => {
                    out.push(&rest[..q]);
                    rest = &rest[q + 1..];
                }
                None => break,
            }
        }
        out
    }

    /// CSS 类名的形状：ASCII 字母开头，其后是字母/数字/`-`/`_`。
    ///
    /// 用它过滤掉中文文案、纯符号、`-` 之类——那些出现在同一行只是因为
    /// 它们是别的参数，不是类名。
    fn looks_like_class(tok: &str) -> bool {
        let mut cs = tok.chars();
        match cs.next() {
            Some(c) if c.is_ascii_alphabetic() => {}
            _ => return false,
        }
        cs.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    }

    /// 从 helper 的 `return '…'` 里补上动态拼接的那一半类名。
    fn css_classes_from_helpers() -> BTreeSet<String> {
        let src = strip_js_comments(script_block());
        let mut out = BTreeSet::new();
        let mut from = 0;
        while let Some(p) = src[from..].find("return '") {
            let s = from + p + "return '".len();
            match src[s..].find('\'') {
                Some(q) => {
                    let body = &src[s..s + q];
                    // 只收看起来像类名列表的：全小写字母/连字符/空格，且**首字符是字母**。
                    //
                    // 首字符那一条不是多余的：`statusLabel` 等函数里有 `return '-'`
                    // （表示"无数据"的展示文本），只查字符集合的话它会被当成一个叫 `-`
                    // 的类名收进来，然后 every_css_class_used_is_defined 报"用了未定义的
                    // 类 `-`"——一个根本不存在的问题，而真正的失败会被埋在噪音里。
                    if body.chars().next().is_some_and(|c| c.is_ascii_lowercase())
                        && body
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c == '-' || c == ' ')
                    {
                        for tok in body.split_whitespace() {
                            out.insert(tok.to_string());
                        }
                    }
                    from = s + q;
                }
                None => break,
            }
        }
        out
    }

    /// 找出 `<style>` 里定义过的所有类选择器。
    ///
    /// 【类名必须以字母开头】CSS 里遍地是 `rgba(76,141,255,.35)`、`opacity:.45`
    /// 这样的小数，`.35` / `.45` 也长得像类选择器。第一版没加这条限制，于是
    /// "定义了但没用"的清单里冒出 `["1","12","35","4","45","5","7","85"]`——
    /// 测试红了，但红的原因跟页面毫无关系。真实的 CSS 类不可能以数字开头
    /// （合法标识符的首字符只能是字母、`_` 或 `-`），按这条筛掉即可。
    fn css_classes_defined() -> BTreeSet<String> {
        let css = strip_css_comments(style_block());
        let css = css.as_str();
        let b = css.as_bytes();
        let mut out = BTreeSet::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'.' {
                let s = i + 1;
                // 首字符必须是字母或下划线，否则这个 `.` 属于小数而非选择器。
                if s >= b.len() || !(b[s].is_ascii_alphabetic() || b[s] == b'_') {
                    i += 1;
                    continue;
                }
                let mut e = s;
                while e < b.len() && (b[e].is_ascii_alphanumeric() || b[e] == b'-' || b[e] == b'_')
                {
                    e += 1;
                }
                out.insert(css[s..e].to_string());
                i = e;
            } else {
                i += 1;
            }
        }
        out
    }

    // ============ 检查 ============

    /// `$('x')` 引用的每个 id 都必须真的存在于 HTML。
    ///
    /// 【这条挡的是什么】实测踩过：JS 调 `$('summary')` 而 HTML 里没有那个容器，
    /// `null.textContent = ''` 抛 TypeError，被 loadKeys 的 catch 收敛成
    /// "网络错误，请重试"——用户看到的提示与真实原因毫无关系，排查时方向全错。
    #[test]
    fn every_js_id_exists_in_html() {
        let mut refs = js_id_refs();
        // 间接引用（`blk('dash-x')` 这类把 id 当实参传的）也算——看板的六个区块
        // 全走这条路，不合进来的话它们的 id 错字对本检查完全隐形。
        let indirect = js_id_refs_via_params();
        assert!(
            indirect.contains("dash-tiers"),
            "对照组失败：间接引用解析应能找到 dash-tiers，实际 {indirect:?}"
        );
        refs.extend(indirect);
        let ids = html_ids();

        // 对照组：解析器必须真的提取到了符号。提不到时下面的循环一次都不执行，
        // 测试照样绿——那是假绿，比失败更难发现。
        assert!(
            refs.len() >= 10,
            "解析失效：只提取到 {} 个 $() 引用（页面至少有十几个）",
            refs.len()
        );
        assert!(ids.contains("wallet"), "对照组：id 解析应能找到 wallet");

        let missing: Vec<_> = refs.difference(&ids).collect();
        assert!(
            missing.is_empty(),
            "JS 引用了不存在的 id：{missing:?}\n（会在运行时抛 TypeError，且被 catch 伪装成网络错误）"
        );
    }

    /// 用到的每个 CSS 类都必须在 `<style>` 里定义过。
    ///
    /// 【这条挡的是什么】同样实测踩过两次：CSS 写 `.sm{display:flex}` 而 HTML 用
    /// `class="summary"`，以及 JS 设 `'spark-bar'` 而 CSS 定义 `.spark-b`。
    /// 两次都是"JS 跑通、无报错、布局静默错位"，只能靠截图发现。
    #[test]
    fn every_css_class_used_is_defined() {
        let mut used = css_classes_used();
        used.extend(css_classes_from_helpers());
        let defined = css_classes_defined();

        assert!(
            used.len() >= 15,
            "解析失效：只提取到 {} 个在用的类名",
            used.len()
        );
        assert!(defined.contains("board"), "对照组：CSS 解析应能找到 .board");

        let missing: Vec<_> = used.difference(&defined).collect();
        assert!(
            missing.is_empty(),
            "用到但未定义的 CSS 类：{missing:?}\n（样式静默失效，页面不报错但布局是错的）"
        );
    }

    /// 定义了却没人用的 CSS 类应当清掉。
    ///
    /// 这条不是洁癖：死规则会让下一个人以为某个类"应该"生效，从而按它的语义写代码，
    /// 而实际上那个类名从未出现在任何元素上。本任务里 `.seats` / `.wallet-unit`
    /// 就是我自己加了却没用上的，靠这条检查才发现。
    #[test]
    fn no_dead_css_classes() {
        let mut used = css_classes_used();
        used.extend(css_classes_from_helpers());
        let defined = css_classes_defined();

        let dead: Vec<_> = defined.difference(&used).collect();
        assert!(
            dead.is_empty(),
            "定义了但没用的 CSS 类：{dead:?}\n（要么用起来，要么删掉——留着会误导下一个人）"
        );
    }

    /// 定义了却没人调用的 JS 函数也应当清掉。
    ///
    /// 【为何补这一条】前面四条检查的是 CSS 类和 DOM id，唯独没管 JS 函数。
    /// 把用户页的运维列（RPM / 走势 / 成功率 / Tokens / 延迟 / 最后使用）删掉之后，
    /// `spark` / `pct` / `statusLabel` / `statusClass` / `tok` / `cdText` / `rfc`
    /// 七个函数一起变成了死代码，而那四条检查全绿——只有临时手写脚本才发现。
    /// 死函数比死 CSS 更能误导人：它们看起来是「页面在用的能力」，下一个人会
    /// 照着它们的语义写新代码，或者花时间维护根本不会执行的分支。
    ///
    /// 只统计本页定义的函数，浏览器内建（`document.*`、数组方法等）不在此列。
    #[test]
    fn no_dead_js_functions() {
        let src = strip_js_comments(script_block());

        // 收集 `function name(` 形式的定义。
        let mut defined = BTreeSet::new();
        let mut from = 0;
        while let Some(p) = src[from..].find("function ") {
            let s = from + p + "function ".len();
            let rest = &src[s..];
            let end = rest
                .find('(')
                .map(|i| s + i)
                .unwrap_or_else(|| src.len().min(s));
            let name = src[s..end].trim();
            if !name.is_empty()
                && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
            {
                defined.insert(name.to_string());
            }
            from = end.max(s);
        }

        // 对照组：解析器必须真的找到了函数。若某次重构让上面的模式失配，
        // `defined` 会变成空集，下面的差集自然为空、测试假绿。
        assert!(
            defined.len() >= 10,
            "对照组失败：只解析到 {} 个函数定义，解析器大概失效了",
            defined.len()
        );
        assert!(
            defined.contains("render") && defined.contains("board"),
            "对照组失败：应能解析到 render / board"
        );

        // 逐个数「除定义处之外还出现过几次」。出现 0 次即无人调用。
        //
        // 【必须按标识符边界数，不能用子串】最初这里写的是 `src.matches(name)`，
        // 于是 `cred` 被 `credits` / `credentialId` / `creditsEnabled` 里的字符命中，
        // 明明已经没人调用却显示"在用"——测试假绿，而假绿比没有测试更糟：
        // 它让我以为清理干净了。判定一个标识符出现，要求它前后都不是标识符字符。
        let ident_count = |hay: &str, name: &str| -> usize {
            let is_id = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '$';
            let b: Vec<char> = hay.chars().collect();
            let n: Vec<char> = name.chars().collect();
            let mut hits = 0;
            let mut i = 0;
            while i + n.len() <= b.len() {
                if b[i..i + n.len()] == n[..] {
                    let before_ok = i == 0 || !is_id(b[i - 1]);
                    let after_ok = i + n.len() >= b.len() || !is_id(b[i + n.len()]);
                    if before_ok && after_ok {
                        hits += 1;
                    }
                }
                i += 1;
            }
            hits
        };

        let dead: Vec<&String> = defined
            .iter()
            .filter(|name| {
                // 定义处本身也算一次出现，所以「只出现定义次数」== 没人调用。
                let defs = src.matches(format!("function {name}(").as_str()).count();
                ident_count(&src, name) <= defs
            })
            .collect();

        assert!(
            dead.is_empty(),
            "定义了但没人调用的 JS 函数：{dead:?}\n（删列/改版后最容易留下这种残骸）"
        );
    }

    /// **安全红线：整个脚本里不得出现 `innerHTML`。**
    ///
    /// 凭据的备注、邮箱、错误原因都可能含 `<`。用 innerHTML 就是把服务端数据
    /// 当代码执行，而这个页面**面向公网**且展示的正是可用凭据——XSS 在这里
    /// 等于把 key 直接送给攻击者。
    #[test]
    fn no_inner_html_usage() {
        let src = strip_js_comments(script_block());

        // 对照组：剔注释后脚本主体必须还在。若 strip 把整段吃掉了，
        // 下面的断言在空串上必然通过，等于没测。
        assert!(
            src.contains("function render"),
            "对照组失败：剔注释后脚本主体没了，解析器有问题"
        );
        assert!(
            !src.contains("绝不用"),
            "对照组失败：注释没被剔掉，innerHTML 检查会被自己的警示文字绊倒"
        );

        assert!(
            !src.contains("innerHTML"),
            "脚本里出现了 innerHTML——凭据数据含 < 时会被当代码执行（XSS）"
        );
        assert!(
            !src.contains("outerHTML"),
            "脚本里出现了 outerHTML——同 innerHTML，一样是注入面"
        );
        assert!(
            !src.contains("document.write"),
            "脚本里出现了 document.write——同样把数据当标记解析"
        );
    }

    /// 明文 key 绝不能进 URL 或 localStorage。
    ///
    /// URL 会进浏览器历史、反代日志、Referer；localStorage 能被任意同源脚本读到。
    /// 明文只应活在当前 DOM 里，页面一关就没。
    #[test]
    fn plaintext_never_leaves_the_dom() {
        let src = strip_js_comments(script_block());
        for bad in ["localStorage", "sessionStorage", "document.cookie"] {
            assert!(
                !src.contains(bad),
                "脚本里出现了 {bad}——明文 key 不得离开当前 DOM"
            );
        }
        // 上车请求把 id 拼进路径是可以的（那不是明文），但必须过 encodeURIComponent。
        assert!(
            src.contains("encodeURIComponent"),
            "拼接 URL 路径时必须 encodeURIComponent"
        );
    }

    /// 页面按钮与后端路由必须使用同一个单号刷新路径。
    #[test]
    fn quota_refresh_uses_the_registered_balance_route() {
        let page = strip_js_comments(script_block());
        let http = include_str!("http.rs");
        let route = "/portal/api/keys/{credential_id}/balance/refresh";

        assert!(http.contains(route), "后端没有注册额度刷新路由 {route}");
        assert!(
            page.contains("'/balance/refresh'"),
            "页面没有请求 balance/refresh，刷新按钮会得到 404"
        );
        assert!(
            !page.contains("'/quota/refresh'"),
            "页面仍在请求旧的 quota/refresh 路径，刷新按钮会得到 404"
        );
    }

    /// **上车成功后就地更新座位区，必须整块重画，不能赋 `textContent`。**
    ///
    /// # 这条挡的是什么
    /// 座位区是「点阵 + 数字」两样东西。表格时代那一格只有一行数字，
    /// 于是 `board()` 里写的是 `tdSeat.textContent = '2/15'`——完全正确。
    /// 改成车位卡之后，同一句话会把点阵抹掉，只剩一行裸数字。
    ///
    /// 【为何非测不可】这个 bug 只在**点过上车按钮之后**出现：页面加载时座位是
    /// 对的（走 render），刷新后又变回对的（重新 render）。唯一看得见的时机是
    /// 上车成功那一瞬，而那时用户的注意力全在「车票出现了」上。截图也难抓——
    /// 得正好在点击后、下一次轮询前截。
    ///
    /// 锁的是「座位区只经 fillSeats 落笔」这件事本身：赋 textContent 就是抹掉。
    #[test]
    fn boarding_repaints_seats_instead_of_overwriting_text() {
        let page = strip_js_comments(script_block());

        let start = page
            .find("function board(")
            .expect("找不到 board()——上车的就地更新逻辑在这里");
        let body = &page[start..];
        let end = body
            .find("\nfunction ")
            .map(|e| e + 1)
            .unwrap_or(body.len());
        let body = &body[..end];

        // 对照组：截出来的确实是 board() 的函数体，而不是空串或整个脚本。
        assert!(
            body.contains("上车中…") && body.len() < 8000,
            "函数体切割失效：截到 {} 字节，下面的断言会变成空检查",
            body.len()
        );

        assert!(
            body.contains("fillSeats("),
            "board() 不再调用 fillSeats：座位点阵不会跟着人数更新"
        );
        for bad in ["seatsBox.textContent =", "seatsBox.textContent="] {
            assert!(
                !body.contains(bad),
                "board() 里给座位区赋 textContent（`{bad}`）会抹掉点阵，只剩一行裸数字——\
                 必须走 fillSeats 整块重画"
            );
        }
    }

    /// 上车成功或服务端告知满员时，车头徽章必须随行内状态立即更新。
    /// 否则只有整表 `render()` 会调用 `fillBadge`，用户要等下一轮刷新才能看到状态。
    #[test]
    fn boarding_repaints_status_badge_without_full_refresh() {
        let page = strip_js_comments(script_block());
        let start = page.find("function board(").expect("找不到 board()");
        let body = &page[start..];
        let end = body
            .find("\nfunction ")
            .map(|e| e + 1)
            .unwrap_or(body.len());
        let body = &body[..end];

        assert!(
            body.matches("fillBadge(cardEl.querySelector('.car-h'), it)")
                .count()
                >= 2,
            "board() 的成功与满员分支都必须立即重画车头徽章"
        );
    }

    /// **车位卡网格的最小轨道宽必须能在窄容器里退让。**
    ///
    /// # 这条挡的是什么
    /// `minmax()` 的下限是**硬**下限：网格轨道不会收缩到它以下。写死
    /// `minmax(320px,1fr)` 时，320 视口减掉 `.wrap` 的左右内边距只剩 288px，
    /// 轨道比容器宽 32px，于是**整页**出现横向滚动条——顶部的余额、退出按钮
    /// 一起被推出屏幕，用户得先左右拖才点得到「退出」。
    ///
    /// `min(320px,100%)` 让下限在窄容器里退到容器宽度，横滚不会发生。
    ///
    /// 【为何测字符串而不是测渲染】这条规则的错法只有一种（把 min() 拆掉），
    /// 而它的后果需要真浏览器 + 320 视口才看得见。锁住写法比锁住像素便宜得多。
    #[test]
    fn card_grid_min_track_yields_in_narrow_containers() {
        let css = strip_css_comments(style_block());

        let line = css
            .lines()
            .find(|l| l.trim_start().starts_with(".cars{"))
            .expect("找不到 .cars 网格定义——车队列表的布局在这里");

        assert!(
            line.contains("min(320px,100%)"),
            "车位卡网格的 minmax 下限必须是 min(320px,100%)：\
             写死 320px 会让 320 视口出现页面级横滚，顶部的退出按钮被推出屏幕。\n实际：{line}"
        );
    }

    /// **服务端能返回的每个 `keyKind`，页面都必须显式处理。**
    ///
    /// # 为何需要这条
    /// 本轮真实翻过一次车：给只读角色加了新的 `keyKind = "forbidden"`，页面没有
    /// 对应分支，于是那些行落到默认分支、显示 `maskedKey`。后果是**五行看起来
    /// 一模一样**（同一个打码串）、指纹全丢——而全部 38 条 API 断言都通过，
    /// 因为服务端确实没下发明文。只有截图才看出不对。
    ///
    /// 光靠"记得同步改页面"守不住：新增一个 keyKind 是服务端的局部改动，
    /// 编译器不会提醒还有个 HTML 字符串里的 if 链等着更新。
    ///
    /// # 为何从源码里抓字面量而不是手写清单
    /// 手写清单会在新增第六种 kind 时漏掉，而漏掉的表现正是这个 bug 本身。
    /// 从 `http.rs` 的 `gate_plaintext` 返回值里抓，新增的 kind 自动进入检查范围。
    #[test]
    fn every_server_key_kind_is_handled_by_the_page() {
        let http_src = include_str!("http.rs");

        // 只扫 gate_plaintext 函数体：那是唯一决定 key_kind 的地方（见它的文档），
        // 全文件扫会把测试里的字面量也算进来。
        let start = http_src
            .find("fn gate_plaintext(")
            .expect("http.rs 里必须有 gate_plaintext");
        let body_end = http_src[start..]
            .find("\nfn now_ms()")
            .expect("gate_plaintext 之后应紧跟 now_ms —— 切分锚点失效了")
            + start;
        let gate_body = &http_src[start..body_end];

        // 抓形如 `"xxx"` 的返回值字面量。gate 的返回类型是 &'static str，
        // 函数体里出现的字符串字面量就是全部可能的 kind。
        let mut kinds = BTreeSet::new();
        let bytes = gate_body.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'"' {
                if let Some(rel) = gate_body[i + 1..].find('"') {
                    let lit = &gate_body[i + 1..i + 1 + rel];
                    // kind 都是纯小写 ascii 单词，借此排除文档里的中文和符号。
                    if !lit.is_empty()
                        && lit.len() <= 16
                        && lit.chars().all(|c| c.is_ascii_lowercase())
                    {
                        kinds.insert(lit.to_string());
                    }
                    i += rel + 2;
                    continue;
                }
            }
            i += 1;
        }

        // 对照组：切分或扫描一旦失效，kinds 会变空/变少，下面的循环就无事可做、
        // 测试假绿。先确认抓到了已知的那几个。
        assert!(
            kinds.len() >= 4,
            "对照组失败：只从 gate_plaintext 抓到 {} 个 key_kind（{:?}），扫描器大概失效了",
            kinds.len(),
            kinds
        );
        for known in ["plain", "locked", "oauth", "none"] {
            assert!(
                kinds.contains(known),
                "对照组失败：没抓到已知的 key_kind {known:?}，实际抓到 {kinds:?}"
            );
        }

        let page_src = strip_js_comments(script_block());

        // `plain` 不参与：页面判的是 `it.key` 有没有值，不比较这个字符串
        // （两者由 key_kind_plain_iff_key_present 锁定同真同假）。
        // `none` 也不参与：它就是"没有可展示的东西"，默认分支正是为它准备的。
        for kind in &kinds {
            if kind == "plain" || kind == "none" {
                continue;
            }
            assert!(
                page_src.contains(&format!("'{kind}'")),
                "服务端会返回 keyKind={kind:?}，但页面脚本里没有处理它的分支。\n\
                 后果：这些行会落到默认分支，显示成与其它行**完全相同**的打码串、\n\
                 且丢掉指纹等本该有的信息，而任何只看 API 响应的测试都发现不了。\n\
                 页面已处理的 kind：{:?}",
                kinds
                    .iter()
                    .filter(|k| page_src.contains(&format!("'{k}'")))
                    .collect::<Vec<_>>()
            );
        }
    }

    /// **看板响应里的每个字段，页面都必须读到。**
    ///
    /// # 为何需要这条
    /// 服务端的看板结构体与这个页面之间**只靠 JSON 字段名耦合**，而字段名不经过
    /// 编译器。给 `DashboardWindow` 加一个 `chargeback` 字段、忘了改页面，结果是
    /// 那个数字在看板上根本不存在——没有报错、没有空白格、没有任何迹象，只是
    /// 运营从此看不到它。`cargo test` 全绿，截图也看不出少了什么（谁会注意到
    /// 一个从未出现过的格子）。
    ///
    /// 反方向（页面读了服务端没有的字段）由运行时的 `undefined` 暴露，且
    /// `num(undefined)` 会显示 0；那是显示错数，比少一格更糟，所以两个方向都查。
    #[test]
    fn every_dashboard_field_is_rendered_by_the_page() {
        let store_src = include_str!("store.rs");
        let page_src = strip_js_comments(script_block());

        // 看板与审计的响应结构体。写死这份名单是有意的：它们是**响应的形状**，
        // 增删一个结构体是显式的设计动作，改这里的成本正好落在该思考的时候。
        // 而字段是零散追加的，那才是会忘的地方，所以字段从源码里抓。
        let structs = [
            "DashboardWindow",
            "KeyHeat",
            "UserTiers",
            "LoginFailRow",
            "IntegrityCheck",
            "DashboardSnapshot",
            // 审计（G4）。`AuditPage` 的 `limit` 正是靠这条查出来没被读：
            // 分页条原先显示的是**前端自己传的** limit，而服务端会 clamp——
            // 传 limit=99999 时屏幕上会写「每页 99999」，实际只有 200 条。
            "AuditPage",
            "AuditEntry",
            "ActionCount",
        ];

        let mut fields: BTreeSet<String> = BTreeSet::new();
        for name in structs {
            let anchor = format!("pub struct {name} {{");
            let start = store_src
                .find(&anchor)
                .unwrap_or_else(|| panic!("store.rs 里找不到 {anchor}——结构体被改名了？"))
                + anchor.len();
            let end = store_src[start..]
                .find("\n}")
                .unwrap_or_else(|| panic!("{name} 的结构体没闭合"))
                + start;
            for line in store_src[start..end].lines() {
                let t = line.trim();
                // 只认 `pub name: Type` 这一形状，注释与属性行自然被排除。
                let Some(rest) = t.strip_prefix("pub ") else {
                    continue;
                };
                let Some((raw, _)) = rest.split_once(':') else {
                    continue;
                };
                let raw = raw.trim();
                if raw.is_empty() || !raw.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
                    continue;
                }
                // serde(rename_all = "camelCase")：下发的名字是驼峰。
                let mut camel = String::new();
                let mut up = false;
                for c in raw.chars() {
                    if c == '_' {
                        up = true;
                    } else if up {
                        camel.push(c.to_ascii_uppercase());
                        up = false;
                    } else {
                        camel.push(c);
                    }
                }
                fields.insert(camel);
            }
        }

        // 对照组：抓不到字段时下面的循环无事可做、测试假绿。先确认几个已知的在里面。
        assert!(
            fields.len() >= 20,
            "对照组失败：只从六个结构体抓到 {} 个字段（{fields:?}），解析器大概失效了",
            fields.len()
        );
        for known in ["tickets", "walletViolations", "loginFails", "unitPrice"] {
            assert!(
                fields.contains(known),
                "对照组失败：没抓到已知字段 {known:?}，实际 {fields:?}"
            );
        }

        // 判定「页面读过这个字段」必须**按标识符边界**，不能用子串包含。
        //
        // 【实测踩过】第一版写的是 `page_src.contains(".refund")`，而页面里有
        // `d.refunded`（上车响应的退款额，与看板无关）。于是把看板的「退款」那一格
        // 整行删掉，测试照样绿——`.refund` 被 `.refunded` 里的字符命中了。
        // 假绿比没有测试更糟：它让我以为这条契约守得住。
        let reads_field = |f: &str| -> bool {
            let pat = format!(".{f}");
            let is_id = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '$';
            let mut from = 0;
            while let Some(p) = page_src[from..].find(&pat) {
                let at = from + p;
                let after = page_src[at + pat.len()..].chars().next();
                // 后一个字符不能是标识符字符（那说明命中的是更长的字段名）。
                if !after.is_some_and(is_id) {
                    return true;
                }
                from = at + pat.len();
            }
            false
        };

        let unread: Vec<&String> = fields.iter().filter(|f| !reads_field(f)).collect();
        assert!(
            unread.is_empty(),
            "看板下发了这些字段但页面没有读：{unread:?}\n\
             后果：这些数字在看板上完全不存在——不报错、不留空格、截图也看不出\n\
             少了什么，运营从此看不到它们。要么渲染出来，要么从响应里删掉。"
        );
    }

    /// **每个全大写的模块级状态变量都必须有 `var` 声明。**
    ///
    /// # 为何需要这条
    /// 本轮实测踩过：分页的两个 handler 都读 `LAST_AUDIT.limit`，而那个变量**从未被
    /// 声明也从未被赋值**。后果是点「下一页」抛 `ReferenceError`，翻页彻底不动——
    /// 但异常发生在 click handler 里，不影响页面其它部分，界面看起来完全正常，
    /// 只是按钮点了没反应。八条页面结构性测试当时全绿：它们查 id、类名、函数名、
    /// 字段名，唯独没查**变量**。只有浏览器控制台里那一行 PAGEERROR 暴露了它。
    ///
    /// # 为何只查全大写的
    /// 本页的约定是「模块级可变状态一律全大写」（`LAST_ITEMS` / `WALLET` / `BUSY`…）。
    /// 局部变量与形参是小写，它们的作用域由 `function` 管，漏声明会被 `'use strict'`
    /// 在赋值时当场打出来。全大写这一类才是「跨函数共享、读的地方离写的地方很远」
    /// 的，也正是会忘记声明的那一类。
    ///
    /// 浏览器内建（`JSON`）与恰好全大写的字符串字面量（`'POST'`）要排除——
    /// 它们不是本页的状态。
    #[test]
    fn every_module_level_state_var_is_declared() {
        let src = strip_js_comments(script_block());

        // 先把单引号字面量整段抹掉：`method: 'POST'` 里的 POST 不是变量。
        // 不抹的话它会被当成「用了未声明的变量」，而那是个假警报——假警报会让
        // 下一个人给这条测试加白名单，白名单一开，真正的漏声明就混进去了。
        let mut no_str = String::with_capacity(src.len());
        let mut in_str = false;
        for c in src.chars() {
            if c == '\'' {
                in_str = !in_str;
                continue;
            }
            no_str.push(if in_str { ' ' } else { c });
        }

        let is_id = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '$';

        // 收集所有形如 `ABC_DEF` 的标识符（>=2 字符，全大写字母/数字/下划线）。
        let mut used: BTreeSet<String> = BTreeSet::new();
        let chars: Vec<char> = no_str.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i].is_ascii_uppercase() {
                let start = i;
                while i < chars.len() && is_id(chars[i]) {
                    i += 1;
                }
                let tok: String = chars[start..i].iter().collect();
                let prev_ok = start == 0 || !is_id(chars[start - 1]);
                // 全大写 + 长度 >= 2 + 不是紧跟在标识符字符后面（排除 `xxAB`）。
                // 还要排除紧跟 `.` 的（那是属性名，如 `d.OK`，不是模块变量）。
                let is_prop = start > 0 && chars[start - 1] == '.';
                if prev_ok
                    && !is_prop
                    && tok.len() >= 2
                    && tok
                        .chars()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                {
                    used.insert(tok);
                }
                continue;
            }
            i += 1;
        }

        // 收集 `var NAME` 的声明。
        let mut declared: BTreeSet<String> = BTreeSet::new();
        let mut from = 0;
        while let Some(p) = no_str[from..].find("var ") {
            let s = from + p + "var ".len();
            let rest: String = no_str[s..].chars().take_while(|c| is_id(*c)).collect();
            if !rest.is_empty() {
                declared.insert(rest);
            }
            from = s;
        }

        // 对照组：扫描器必须真的认出了已知的状态变量。认不出时下面的差集为空、
        // 测试假绿——而假绿正是本条测试要防的那种情况。
        for known in ["LAST_ITEMS", "WALLET", "AUDIT_Q", "LAST_AUDIT"] {
            assert!(
                used.contains(known),
                "对照组失败：没扫到已知状态变量 {known:?}，实际扫到 {used:?}"
            );
            assert!(
                declared.contains(known),
                "对照组失败：没扫到 {known:?} 的 var 声明，实际声明 {declared:?}"
            );
        }

        // 浏览器内建，不是本页状态。
        const BUILTIN: &[&str] = &["JSON", "NaN", "URL"];

        let missing: Vec<&String> = used
            .iter()
            .filter(|v| !declared.contains(*v) && !BUILTIN.contains(&v.as_str()))
            .collect();
        assert!(
            missing.is_empty(),
            "这些全大写变量被使用但没有 var 声明：{missing:?}\n\
             后果：读它就是运行时 ReferenceError。若发生在 click handler 里，\n\
             表现是「按钮点了没反应」——页面其余部分完全正常，界面上毫无迹象，\n\
             只有浏览器控制台有一行报错。实测本轮的 LAST_AUDIT 就是这样漏的。"
        );
    }

    /// **服务端接受的每个审计筛选参数，页面都必须发得出来。**
    ///
    /// # 为何需要这条
    /// `actionPrefix` 就是这样漏的：store 层实现了、http 层解析了、五条测试锁了它的
    /// 行为，但 `readFilters()` 从来不读它，HTML 里也没有对应输入框——于是只有 API
    /// 调用者用得上，界面上那个能力**根本不存在**。全部测试绿，因为没有一条测试
    /// 关心「界面能不能触达这个参数」。
    ///
    /// 这类缺口不会报错、不留空白、截图也看不出来（谁会注意到一个从未出现过的输入框），
    /// 而它的代价是运营要把「看所有登录失败」拆成四次精确筛选。
    ///
    /// `offset` 与 `limit` 不算筛选维度：前者由翻页按钮算，后者是「每页」下拉，
    /// 两者都不该有独立输入框。它们仍必须出现在 `auditQs` 里，这一点单独断言。
    #[test]
    fn every_audit_filter_param_is_reachable_from_the_page() {
        let http_src = include_str!("http.rs");
        let page_src = strip_js_comments(script_block());

        // 从 AuditParams 结构体抓字段名（snake_case），转成 JSON 的 camelCase。
        let anchor = "struct AuditParams {";
        let start = http_src
            .find(anchor)
            .expect("http.rs 里找不到 AuditParams——结构体被改名了？")
            + anchor.len();
        let end = start
            + http_src[start..]
                .find('}')
                .expect("AuditParams 结构体没有闭合括号");

        let mut params: Vec<String> = Vec::new();
        for line in http_src[start..end].lines() {
            let line = line.trim();
            if line.starts_with("//") || line.starts_with("#[") || line.is_empty() {
                continue;
            }
            if let Some(name) = line.split(':').next() {
                let name = name.trim();
                if name.is_empty() {
                    continue;
                }
                // snake_case → camelCase（serde rename_all = "camelCase"）
                let mut camel = String::new();
                let mut upper_next = false;
                for c in name.chars() {
                    if c == '_' {
                        upper_next = true;
                    } else if upper_next {
                        camel.push(c.to_ascii_uppercase());
                        upper_next = false;
                    } else {
                        camel.push(c);
                    }
                }
                params.push(camel);
            }
        }
        assert!(
            params.len() >= 7,
            "只抓到 {} 个 AuditParams 字段，解析逻辑坏了: {params:?}",
            params.len()
        );

        // 对照组：解析必须真的认出了 camelCase 转换，否则这条测试恒绿。
        assert!(
            params.contains(&"actionPrefix".to_string()),
            "对照组失败：没抓到 actionPrefix（snake→camel 转换坏了）: {params:?}"
        );

        // 每个参数都必须出现在 auditQs 的拼串里（那是唯一发查询串的地方）。
        let qs_start = page_src
            .find("function auditQs(")
            .expect("页面里找不到 auditQs——函数被改名了？");
        let qs_end = qs_start + page_src[qs_start..].find("\n}").expect("auditQs 没有闭合");
        let qs_body = &page_src[qs_start..qs_end];

        let unsent: Vec<&String> = params
            .iter()
            .filter(|p| !qs_body.contains(&format!("'{p}=")))
            .collect();
        assert!(
            unsent.is_empty(),
            "服务端接受这些审计筛选参数，但 auditQs 从不发送：{unsent:?}\n\
             后果：这个筛选能力只有 API 调用者用得上，界面上不存在——不报错、\n\
             不留空白、截图也看不出来。实测 actionPrefix 就是这样漏了一整轮。"
        );

        // 筛选维度（offset/limit 之外的）还必须能被用户**输入**：readFilters 要读它。
        let rf_start = page_src
            .find("function readFilters(")
            .expect("页面里找不到 readFilters");
        let rf_end = rf_start
            + page_src[rf_start..]
                .find("\n}")
                .expect("readFilters 没有闭合");
        let rf_body = &page_src[rf_start..rf_end];

        let no_input: Vec<&String> = params
            .iter()
            .filter(|p| p.as_str() != "offset" && p.as_str() != "limit")
            .filter(|p| !rf_body.contains(p.as_str()))
            .collect();
        assert!(
            no_input.is_empty(),
            "这些筛选参数 readFilters 没有读，界面上没有对应输入控件：{no_input:?}\n\
             （offset 由翻页按钮算、limit 是「每页」下拉，两者已豁免）"
        );
    }

    /// **结束时间必须走补末尾的那个函数，起始时间必须不走。**
    ///
    /// # 为何需要这条
    /// `<input type="datetime-local">` 不带 `step` 时粒度是**分钟**，而 SQL 的上界是
    /// 闭区间 `at_ms <= until_ms`。两者相乘的后果是：用户选「到 22:00」，实际发出
    /// `22:00:00.000`，于是 `22:00:30` 那条被排除——最多静默漏掉 59.999 秒的记录，
    /// 而界面上没有任何迹象说漏了。审计是拿去当证据的，「少了一条」比「多了一条」
    /// 危险得多。
    ///
    /// 这条测试锁的是**两个方向**：
    /// - `until` 走 `localToMsEnd`（补到那一分钟的末尾）
    /// - `since` 走裸 `localToMs`（下界要的就是那一分钟的开头，补了反而漏掉开头 59 秒）
    ///
    /// 把两者写反或写成同一个函数，界面照样跑、照样返回数据，只是边界悄悄错一分钟。
    #[test]
    fn audit_end_of_window_is_padded_but_start_is_not() {
        let page_src = strip_js_comments(script_block());

        let rf_start = page_src
            .find("function readFilters(")
            .expect("页面里找不到 readFilters");
        let rf_end = rf_start
            + page_src[rf_start..]
                .find("\n}")
                .expect("readFilters 没有闭合");
        let rf = &page_src[rf_start..rf_end];

        // until 必须用补末尾的版本。
        assert!(
            rf.contains("localToMsEnd($('f-until')"),
            "结束时间没走 localToMsEnd。\n\
             后果：datetime-local 只到分钟，而 SQL 上界是闭区间——用户选「到 22:00」\n\
             会把 22:00:01–22:00:59 的记录静默排除在外。\n\
             readFilters 实际内容：{rf}"
        );
        // since 必须**不**用它。
        assert!(
            rf.contains("localToMs($('f-since')"),
            "起始时间应当用裸 localToMs（下界要那一分钟的开头）: {rf}"
        );
        assert!(
            !rf.contains("localToMsEnd($('f-since')"),
            "起始时间误用了 localToMsEnd——那会把所选那一分钟的头 59 秒漏掉: {rf}"
        );

        // 补偿量必须覆盖整分钟（59999），而不是只补毫秒（999）。
        let fn_start = page_src
            .find("function localToMsEnd(")
            .expect("页面里找不到 localToMsEnd");
        let fn_end = fn_start
            + page_src[fn_start..]
                .find("\n}")
                .expect("localToMsEnd 没有闭合");
        let body = &page_src[fn_start..fn_end];
        assert!(
            body.contains("59999"),
            "localToMsEnd 没有补满一整分钟（应含 59999）。\n\
             只补 999 毫秒的话，分钟粒度输入仍会漏掉 59 秒的记录: {body}"
        );
        assert!(
            body.contains("999"),
            "带秒的输入（step=1）也要补到 .999，否则漏掉那一秒内的毫秒: {body}"
        );
    }
}
