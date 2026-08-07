import { useEffect, useRef, useState } from 'react'

/**
 * FireCanvas —— 移植自 claude-ultracode-effort-card 的 WebGL2 火焰(原版 shader,丝滑不打折)。
 *
 * ⭐架构关键(数百号不崩):**每个实例自带一个 WebGL2 上下文**,而浏览器对并发上下文有硬上限
 * (WebKit/Chrome 都在 16 附近),超限后会**强制丢弃最老的上下文**。被丢弃的 canvas 静默变空白,
 * 但 RAF 若照常调度,就会每帧对着一个死上下文发一整套 GL 命令(4 个 draw call + 纹理绑定):
 * 画面没了,开销全在。这正是实测到的「WebKit.GPU 进程 102% 而 WebContent 0.0%」——
 * 忙的是合成器不是 JS;刷掉那个标签后 GPU 从 102% 掉到 7%。故本文件承担两道闸:
 *   ① `pickFireCandidates`(见下)把同时点火的实例数**硬顶在 MAX_FIRE_INSTANCES**,
 *      调用方传多大 max 都突破不了。数量是根因,这道闸是主力。
 *   ② `webglcontextlost` 监听 + 每帧 `isContextLost()` 双保险:真被丢弃时**立刻停掉 RAF**,
 *      不再对死上下文空转。
 * 号一旦不再点火,组件卸载 → 上下文销毁释放 → 零常驻开销。
 *
 * 三段管线(与 effort card 一致):sim(火焰模拟,读回上一帧做拖尾衰减)→ 高斯 blur → composite(辉光合成)。
 * 火焰沿条形轨道从右向左燃烧铺满(slider 恒 1.0 = 满档火力)。页面隐藏时暂停 RAF 省电。
 * WebGL2 不可用则自动降级(什么都不渲染,不报错;StatusBars 下方仍有 CSS 兜底高亮)。
 */

// ══ 火焰实例预算(纯逻辑,单测在 admin-ui/tests/fire-candidates.test.ts) ══

/**
 * 同时最多几个 FireCanvas 真的持有 WebGL 上下文。
 *
 * 4 不是拍脑袋:并发上下文硬上限 ~16,超限即强制丢弃最老的;而每个实例还要 3 套 shader + 4 个 FBO
 * 且带 blur/composite 全屏 pass,GPU 侧成本随实例数线性涨。留足余量给页面上别的 canvas 用途。
 */
export const MAX_FIRE_INSTANCES = 4

/** 排序与滞后判定所需的单号活动快照(与 CredentialStatusItem 解耦,便于单测)。 */
export interface FireCandidate {
  id: number
  /** 后端 /ratelimit/insights 判定的 RPM 饱和 —— 最硬的信号,优先级最高。 */
  saturated: boolean
  inflight: number
  rpm: number
  /** 非禁用号(healthOf !== 'disabled')。禁用号一律不点火。 */
  lit: boolean
}

// 点火门槛(判据与原 StatusBars 的 onFire 一致,只是搬进来一并单测)。
const FIRE_INFLIGHT_GATE = 2
const FIRE_RPM_GATE = 20

// ── 滞后(hysteresis)阈值:承重逻辑 ──
// 为什么必须有:`getContext('webgl2')` 本身开销不小(编译三套 shader + 建四个 FBO),反复创建销毁
// 比一直开着更贵。若只按瞬时强度取前 N,inflight/rpm 的正常抖动就会让火焰在几个号之间反复
// 挂载/卸载 —— 压住了数量却换来更差的抖动。故:**在烧的号占着位子,挑战者要明显更猛才换得掉**。
// 取值依据:inflight 典型抖动 ±1,故要求多出 2 才算真更猛;rpm 是 60s 滑窗、抖动个位数,故要求多出 15。
const HYSTERESIS_INFLIGHT_MARGIN = 2
const HYSTERESIS_RPM_MARGIN = 15

/** 是否够资格点火:非禁用 + (后端判饱和 | 在途打满 | RPM 打得猛)任一。 */
export function isFireEligible(c: FireCandidate): boolean {
  return c.lit && (c.saturated || c.inflight >= FIRE_INFLIGHT_GATE || c.rpm >= FIRE_RPM_GATE)
}

/**
 * 强度排序键:饱和 > 在途 > RPM > id。
 *
 * 依据:`saturated` 是后端按 rpm_limit 算出的**结论**(含 rpm_limit=0 时的高水位兜底),比前端拿到的
 * 两个瞬时数更可信;`inflight` 是"此刻真的在打",比 60s 滑窗的 rpm 更即时;`rpm` 最后作量级参考。
 * 末位用 id 升序兜底,保证这是个**全序**:任何两个号都有确定名次,结果不随输入顺序或 sort 实现而抖
 * (抖 = 反复重建上下文,见上)。
 */
function compareStrength(a: FireCandidate, b: FireCandidate): number {
  if (a.saturated !== b.saturated) return a.saturated ? -1 : 1
  if (a.inflight !== b.inflight) return b.inflight - a.inflight
  if (a.rpm !== b.rpm) return b.rpm - a.rpm
  return a.id - b.id
}

/** 挑战者是否**明显**强于在位者(滞后判定:差距不明显就维持现状)。 */
function isDecisivelyStronger(challenger: FireCandidate, holder: FireCandidate): boolean {
  // 饱和翻转是后端下的硬结论,不设滞后立刻换 —— 正在挨限流的号才是用户最该看见的那个。
  if (challenger.saturated !== holder.saturated) return challenger.saturated
  if (challenger.inflight >= holder.inflight + HYSTERESIS_INFLIGHT_MARGIN) return true
  if (challenger.inflight !== holder.inflight) return false
  return challenger.rpm >= holder.rpm + HYSTERESIS_RPM_MARGIN
}

/**
 * 从全部号里挑出真正挂 FireCanvas 的那几个(≤ MAX_FIRE_INSTANCES),其余退回 CellFlow 格子流。
 *
 * @param burning 上一帧真在烧的 id 集合 —— 滞后判定的输入。不传则视为冷启动(纯按强度取前 N)。
 */
export function pickFireCandidates(
  candidates: readonly FireCandidate[],
  burning: ReadonlySet<number> = new Set<number>(),
  max: number = MAX_FIRE_INSTANCES,
): number[] {
  // 硬顶:上下文预算是本文件的责任,不是调用方的 —— 传 99 也只给 MAX_FIRE_INSTANCES。
  const cap = Math.max(0, Math.min(Math.floor(max) || 0, MAX_FIRE_INSTANCES))
  if (cap === 0) return []
  const eligible = candidates.filter(isFireEligible).sort(compareStrength)
  if (eligible.length <= cap) return eligible.map((c) => c.id)

  // 名额有争抢:在位者(仍够资格的)按名次先占位,空出来的名额给名次最高的挑战者。
  const held = eligible.filter((c) => burning.has(c.id)).slice(0, cap)
  const newcomers = eligible.filter((c) => !burning.has(c.id))
  const free = cap - held.length
  const picked = [...held, ...newcomers.slice(0, free)]
  // 落选的挑战者只能靠"明显更猛"抢在位者的位子。newcomers 已按名次降序 ⇒ 第一个抢不动的
  // 后面更抢不动,直接停(结果因此不依赖遍历深度)。
  for (const n of newcomers.slice(free)) {
    let weakest = -1
    for (let i = 0; i < picked.length; i++) {
      // 只有在位者的位子受滞后保护;本轮刚补进来的新号不该被同轮的下一个新号反复顶替。
      if (!burning.has(picked[i].id)) continue
      if (weakest < 0 || compareStrength(picked[i], picked[weakest]) > 0) weakest = i
    }
    if (weakest < 0 || !isDecisivelyStronger(n, picked[weakest])) break
    picked[weakest] = n
  }
  return picked.sort(compareStrength).map((c) => c.id)
}

const VERT = `#version 300 es
layout(location=0) in vec2 a_pos;
out vec2 v_uv;
void main(){ v_uv=a_pos*0.5+0.5; gl_Position=vec4(a_pos,0.0,1.0); }`

// 火焰模拟(effort card FRAG_SIM 原样;u_slider 恒 1.0 表示满档火力)
const FRAG_SIM = `#version 300 es
precision highp float;
in vec2 v_uv; out vec4 fc;
uniform float u_time, u_slider, u_elapsed;
uniform vec3 u_ember, u_glow, u_core;
uniform sampler2D u_back;
float hash(vec2 p){ return fract(sin(dot(p,vec2(127.1,311.7)))*43758.5453); }
void main(){
  vec2 uv=v_uv;
  // 栅格密度:窄条(~84×16px)上原版 72×6 每格仅 ~1px 糊成实心,失去 CUDA 点阵感;
  // 粗化到 26×3 → 每格 ~3px,格子清晰可辨(与 effort card 大卡上 72×6 的观感一致)。
  vec2 g=uv*vec2(26.0,3.0);
  vec2 id=floor(g);
  vec2 cf=fract(g);
  float h=hash(id);
  vec2 ap=abs(cf-0.5);
  float cell=smoothstep(0.34,0.22,max(ap.x*0.9,ap.y));
  vec3 prev=texture(u_back,uv).rgb;
  // ⭐恢复 effort card 的温度渐变:左端(火焰前沿)大段软入压暗 → 只有余烬暗红,
  //   右端(u_slider=1)才白热。这道 smoothstep(0,0.35) 正是 image2「左暗右亮」的来源;
  //   之前被压成 (0,0.05) 试图铺满整条,反而把整条冲成均匀亮粉(image1 丑)。
  float fade_mask = smoothstep(0.0, 0.35, uv.x);
  vec3 decay = prev * 0.90 * fade_mask;
  float act=smoothstep(0.95,1.0,u_slider);
  if(act<0.01||u_elapsed<0.0){ fc=vec4(decay,1.0); return; }
  float t=u_time;
  float cellDelay = h * 1.2;
  float cellAge   = max(u_elapsed - cellDelay, 0.0);
  float ignited   = step(0.001, cellAge);
  float cellSpd   = 0.85 + h * 0.30;
  float eased = 1.0 - pow(1.0 - clamp(cellAge / 2.5, 0.0, 1.0), 3.0);
  float dist  = eased * u_slider * cellSpd * ignited;
  float cellOff = (h - 0.5) * 0.05;
  float front   = max(u_slider - dist - cellOff, 0.02);
  float tail    = max(u_slider - front, 0.001);
  float inZ   = step(front - 0.003, uv.x) * step(uv.x, u_slider + 0.003);
  float dn    = clamp(max(u_slider - uv.x, 0.0) / tail, 0.0, 1.0);
  // ⭐恢复陡峭亮度衰减(指数 0.65)+ 低余烬底噪(0.05):离白热核心越远越暗,
  //   形成 暗红余烬(左)→ 粉(中)→ 白热(右) 的连续温度梯度,而非之前的均匀亮。
  float bright = pow(1.0 - dn, 0.65);
  bright = max(bright, 0.05 * ignited) * inZ;
  bright *= 1.0 - smoothstep(0.94, 1.05, dn);
  float es = mix(0.15, 0.5, min(u_elapsed / 1.0, 1.0));
  float vy = abs(uv.y - 0.5) * 2.0;
  float vf = pow(max(1.0 - vy * vy * 0.45, 0.0), 0.75);
  float ts = mix(0.85, 1.0, min(u_elapsed / 1.5, 1.0));
  float f1 = sin(uv.x * 30.0 + t * 15.0 * ts + h * 6.28);
  float f2 = sin(uv.x * 17.0 + t * 8.0 * ts + h * 3.14);
  float f3 = sin(uv.x * 52.0 + t * 25.0 * ts + h * 10.0);
  float flame = smoothstep(0.08, 0.92, (f1 + f2 * 0.5 + f3 * 0.25) * 0.35 + 0.5);
  float r1 = sin(dn * 16.0 - t * 5.0 * ts + h * 3.0);
  float r2 = sin(dn * 8.0 - t * 2.5 * ts + h * 5.0);
  float rhythm = smoothstep(-0.15, 0.55, r1) * (r2 * 0.5 + 0.5);
  rhythm = pow(max(rhythm, 0.0), 1.2);
  float avgSpd = dist / max(cellAge, 0.001);
  float age    = max(cellAge - max(u_slider - uv.x, 0.0) / max(avgSpd, 0.001), 0.0);
  float flash  = step(0.0, age) * exp(-age * 3.2);
  float sp  = fract(t * (0.38 + h * 0.15) + h * 7.0);
  float sX  = u_slider - sp * tail;
  float sY  = 0.5 + sin(sp * 11.0 + h * 6.28) * 0.28;
  float spark = smoothstep(0.014, 0.0, abs(uv.x - sX))
              * smoothstep(0.18, 0.0, abs(uv.y - sY))
              * (1.0 - sp) * (1.0 - sp) * es;
  float energy = bright * vf * (flame * 0.42 + rhythm * 0.38)
               + flash * bright * vf * 0.55
               + spark * 0.7 * inZ;
  energy *= es;
  float edgeBase = exp(-pow((uv.x - front) * 18.0, 2.0));
  float ef1 = sin(uv.x * 45.0 + t * 20.0 * ts + h * 6.28) * 0.5 + 0.5;
  float ef2 = sin(uv.x * 28.0 + t * 11.0 * ts + h * 3.14) * 0.5 + 0.5;
  float edge = edgeBase * (0.25 + ef1 * ef2 * 1.5) * 1.6 * act * es;
  float leadD    = front - uv.x;
  float leadZone = smoothstep(0.07, 0.0, leadD) * step(0.0, leadD) * vf;
  float h2       = hash(id + vec2(99.0, 33.0));
  float leadF    = sin(leadD * 100.0 + t * 20.0 * ts + h2 * 6.28) * 0.5 + 0.5;
  float leadSpark = leadZone * step(0.6, h2) * leadF * act * es * 0.5;
  float total = energy + edge + leadSpark;
  vec3 ember = u_ember;
  vec3 wpur  = u_glow;
  vec3 wht   = u_core;
  float temp = 1.0 - dn;
  vec3 col   = mix(ember, wpur, temp);
  col        = mix(col, wht, pow(temp, 4.5));
  col       *= total;
  // 注:原 effort card 在 u_slider(滑块)位置叠了一枚白色核心高光——那是"滑块 thumb"的视觉。
  // 我们是满档火焰、没有滑块,u_slider=1.0 会让白芯固定糊在最右边缘变成一个白点(dwgx:去掉白点)。
  // 故移除该滑块位置的核心白芯 + 边缘紫光,只保留火焰本身。
  col *= cell;
  col *= fade_mask;
  fc = vec4(min(decay + col, vec3(1.5)), 1.0);
}`

const FRAG_BLUR = `#version 300 es
precision highp float;
in vec2 v_uv; out vec4 fc;
uniform sampler2D u_tex;
uniform vec2 u_dir, u_res;
uniform float u_ext;
vec3 s(vec2 uv){
  vec3 c=texture(u_tex,uv).rgb;
  return u_ext>0.5 && dot(c,vec3(0.2126,0.7152,0.0722))<0.3 ? vec3(0.0) : c;
}
void main(){
  vec2 o=u_dir*1.8/u_res;
  vec3 r=s(v_uv)*0.227027;
  r+=s(v_uv+o)*0.194595;    r+=s(v_uv-o)*0.194595;
  r+=s(v_uv+o*2.0)*0.121622;r+=s(v_uv-o*2.0)*0.121622;
  r+=s(v_uv+o*3.0)*0.054054;r+=s(v_uv-o*3.0)*0.054054;
  fc=vec4(r,1.0);
}`

// 辉光合成 → 屏幕。⭐这里输出**预乘 alpha**,不再靠 CSS mixBlendMode:'screen' 去黑底。
// 依据:mix-blend-mode 会让合成器无法把这层缓存成独立 layer —— 每帧都得把 canvas 和它背后的
// 整片背景重新混合一次,N 个火焰就是 N 次全层重合成,全落在 GPU 进程(实测 GPU 102% 的直接来源)。
// 改法:alpha = max(r,g,b)(暗处→0 全透,亮处→1),RGB 已是最终亮度即天然预乘,浏览器按
// premultipliedAlpha 做普通 source-over:dst = src + dst*(1-a)。
// 与 screen(dst = s + d*(1-s),逐通道)的差别:alpha 是标量,只能取三通道最大值,故暖色火焰
// (如 1.0,0.2,0.1)在弱通道上会比 screen 稍多遮住背景一点 —— 本条带背景本就是近黑底色,肉眼难辨。
const FRAG_COMP = `#version 300 es
precision highp float;
in vec2 v_uv; out vec4 fc;
uniform sampler2D u_scene, u_glow;
void main(){
  vec3 s=texture(u_scene,v_uv).rgb;
  vec3 g=texture(u_glow,v_uv).rgb;
  vec3 c=1.0-exp(-(s+g*1.2+s*g*0.35)*1.15);
  fc=vec4(c, clamp(max(max(c.r,c.g),c.b),0.0,1.0));
}`

type Triad = { ember: [number, number, number]; glow: [number, number, number]; core: [number, number, number] }

// 火焰强度分级配色 —— effort card 7 主题按「越强越高级」全上:
// Arc Cyan → Aurora Green → Solar Gold → Ember Orange → Original Violet → Ice White(白热) → Ruby Pulse(满档最红)。
// 越升越亮直到白热,最后猛然饱和成 Ruby 红。强度 0..1(由 RPM/在途/压力派生),
// 相邻档位平滑插值,颜色随强度连续变化;运行中变化每帧缓动过渡(见 render 循环)。
const FIRE_STOPS: { at: number; c: Triad }[] = [
  // 低强度:Arc Cyan(青,刚起火,克制)
  { at: 0.0, c: { ember: [0.02, 0.22, 0.34], glow: [0.15, 0.78, 1.0], core: [0.88, 1.0, 1.0] } },
  // 中低:Aurora Green(绿)
  { at: 0.2, c: { ember: [0.02, 0.24, 0.10], glow: [0.18, 0.88, 0.42], core: [0.88, 1.0, 0.90] } },
  // 中低偏暖:Solar Gold(金,升温)——绿橙之间新插,effort card 主题原色。
  { at: 0.4, c: { ember: [0.42, 0.24, 0.02], glow: [1.0, 0.72, 0.08], core: [1.0, 0.98, 0.82] } },
  // 中:Ember Orange(橙,Claude 品牌橙)
  { at: 0.6, c: { ember: [0.50, 0.12, 0.03], glow: [1.0, 0.38, 0.10], core: [1.0, 0.93, 0.78] } },
  // 高:Original Violet(紫,Anthropic ultracode 的高档色)
  { at: 0.75, c: { ember: [0.28, 0.10, 0.58], glow: [0.62, 0.32, 1.0], core: [1.0, 0.94, 0.98] } },
  // 高偏白:Ice White(白热,降温闪白过渡)——紫红之间新插,冲向满档前的一记白热。
  { at: 0.88, c: { ember: [0.18, 0.20, 0.26], glow: [0.64, 0.72, 0.86], core: [1.0, 1.0, 1.0] } },
  // 满档:Ruby Pulse(纯红,最强,dwgx 要更红)——glow 去粉调纯红、core 去粉泛白偏暖红。
  { at: 1.0, c: { ember: [0.55, 0.02, 0.04], glow: [1.0, 0.06, 0.08], core: [1.0, 0.82, 0.72] } },
]

const lerp = (a: number, b: number, t: number) => a + (b - a) * t
const lerp3 = (a: [number, number, number], b: [number, number, number], t: number): [number, number, number] =>
  [lerp(a[0], b[0], t), lerp(a[1], b[1], t), lerp(a[2], b[2], t)]

// 按强度 0..1 在色标间插值出目标火焰三色。
function triadForIntensity(level: number): Triad {
  const x = Math.max(0, Math.min(1, level))
  let lo = FIRE_STOPS[0], hi = FIRE_STOPS[FIRE_STOPS.length - 1]
  for (let i = 0; i < FIRE_STOPS.length - 1; i++) {
    if (x >= FIRE_STOPS[i].at && x <= FIRE_STOPS[i + 1].at) { lo = FIRE_STOPS[i]; hi = FIRE_STOPS[i + 1]; break }
  }
  const span = hi.at - lo.at || 1
  const t = (x - lo.at) / span
  return { ember: lerp3(lo.c.ember, hi.c.ember, t), glow: lerp3(lo.c.glow, hi.c.glow, t), core: lerp3(lo.c.core, hi.c.core, t) }
}

export interface FireCanvasProps {
  /** 是否点火(false 时不渲染,组件应由父级条件挂载以彻底释放 WebGL 上下文) */
  active: boolean
  /** 火焰强度 0..1:驱动配色分级(青→绿→金→橙→紫→白热→Ruby红,7 档)。默认 1(满档 Ruby)。运行中变化会平滑过渡。 */
  intensity?: number
  className?: string
}

// 上下文丢失后最多重建几次。上限存在的理由:若浏览器在 lost/restored 之间来回抖(上下文名额
// 长期不够时可能发生),无上限的"丢失即重建"会变成一个由浏览器驱动的重建风暴,比空转更糟。
// 用完额度就永久放弃(画面留空,零开销),不再参与抢名额。
const MAX_CONTEXT_RESTORES = 3

export function FireCanvas({ active, intensity = 1, className }: FireCanvasProps) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null)
  const startRef = useRef<number>(0)
  // 目标强度放 ref,render 循环每帧朝它平滑逼近(颜色切换过渡动画),intensity prop 变化不重建 WebGL。
  const targetIntensityRef = useRef<number>(intensity)
  targetIntensityRef.current = intensity
  // 上下文丢失→恢复时靠 bump epoch 重跑整个 useEffect 来重建资源:复用已验证过的初始化路径,
  // 不另写一份"局部重建"逻辑(那份没人跑得到,只会腐烂)。
  const [epoch, setEpoch] = useState(0)
  const restoreCountRef = useRef(0)

  useEffect(() => {
    const canvas = canvasRef.current
    if (!canvas || !active) return
    const gl = canvas.getContext('webgl2', {
      preserveDrawingBuffer: false,
      antialias: false,
      alpha: true,
      // composite shader 输出的 RGB 已是最终亮度、alpha 取其最大通道 ⇒ 天然预乘。显式写出来是
      // 契约声明:改 FRAG_COMP 的 alpha 算法前先回来看这一行(默认值也是 true,但默认值不表达意图)。
      premultipliedAlpha: true,
    })
    if (!gl) return // WebGL2 不可用:静默降级

    startRef.current = performance.now()
    let raf = 0
    let disposed = false
    // 当前强度(每帧朝 targetIntensityRef 缓动逼近 → 配色切换有平滑过渡,不硬跳)
    let curIntensity = targetIntensityRef.current

    const compile = (type: number, src: string) => {
      const sh = gl.createShader(type)!
      gl.shaderSource(sh, src)
      gl.compileShader(sh)
      if (!gl.getShaderParameter(sh, gl.COMPILE_STATUS)) { gl.deleteShader(sh); return null }
      return sh
    }
    const link = (vs: string, fs: string) => {
      const v = compile(gl.VERTEX_SHADER, vs), f = compile(gl.FRAGMENT_SHADER, fs)
      if (!v || !f) return null
      const p = gl.createProgram()!
      gl.attachShader(p, v); gl.attachShader(p, f)
      gl.bindAttribLocation(p, 0, 'a_pos'); gl.linkProgram(p)
      gl.deleteShader(v); gl.deleteShader(f)
      if (!gl.getProgramParameter(p, gl.LINK_STATUS)) return null
      return p
    }

    const simProg = link(VERT, FRAG_SIM)
    const blurProg = link(VERT, FRAG_BLUR)
    const compProg = link(VERT, FRAG_COMP)
    if (!simProg || !blurProg || !compProg) return

    const vao = gl.createVertexArray()!
    gl.bindVertexArray(vao)
    const vbo = gl.createBuffer()!
    gl.bindBuffer(gl.ARRAY_BUFFER, vbo)
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, -1, 1, 1, -1, 1, 1]), gl.STATIC_DRAW)
    gl.enableVertexAttribArray(0)
    gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0)

    const U = {
      time: gl.getUniformLocation(simProg, 'u_time'),
      slider: gl.getUniformLocation(simProg, 'u_slider'),
      elapsed: gl.getUniformLocation(simProg, 'u_elapsed'),
      ember: gl.getUniformLocation(simProg, 'u_ember'),
      glow: gl.getUniformLocation(simProg, 'u_glow'),
      core: gl.getUniformLocation(simProg, 'u_core'),
      back: gl.getUniformLocation(simProg, 'u_back'),
      blurDir: gl.getUniformLocation(blurProg, 'u_dir'),
      blurExt: gl.getUniformLocation(blurProg, 'u_ext'),
      blurTex: gl.getUniformLocation(blurProg, 'u_tex'),
      blurRes: gl.getUniformLocation(blurProg, 'u_res'),
      compScene: gl.getUniformLocation(compProg, 'u_scene'),
      compGlow: gl.getUniformLocation(compProg, 'u_glow'),
    }

    const makeFBO = () => {
      const fbo = gl.createFramebuffer()!, tex = gl.createTexture()!
      gl.bindFramebuffer(gl.FRAMEBUFFER, fbo)
      gl.bindTexture(gl.TEXTURE_2D, tex)
      gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, canvas.width, canvas.height, 0, gl.RGBA, gl.UNSIGNED_BYTE, null)
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR)
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR)
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE)
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE)
      gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, tex, 0)
      gl.clearColor(0, 0, 0, 1); gl.clear(gl.COLOR_BUFFER_BIT)
      return { fbo, tex }
    }

    // 同步 backing store 到 CSS 尺寸;返回是否真的变了(没变就别白重建 FBO)。
    const resize = () => {
      const rect = canvas.getBoundingClientRect()
      const dpr = Math.min(window.devicePixelRatio || 1, 2)
      const w = Math.max(1, Math.round(rect.width * dpr))
      const h = Math.max(1, Math.round(rect.height * dpr))
      if (w === canvas.width && h === canvas.height) return false
      canvas.width = w
      canvas.height = h
      return true
    }
    resize()

    let simA = makeFBO(), simB = makeFBO(), blurH = makeFBO(), blurV = makeFBO()

    // ── 尺寸自适应(P2)──
    // 原先 resize() 只在初始化调一次,四个 FBO 就此定格在当时的像素尺寸;窗口缩放/侧栏折叠后
    // canvas 的 CSS 尺寸变了而 backing store 没变 ⇒ 画面被拉伸模糊。
    // ⚠️ 两处必须小心:① resize 期间 RO 会连着触发几十次,不防抖则"修完更卡" ⇒ 用 RAF 合并到下一帧
    // (一帧内多少次回调都只重建一次);② 重建前必须删掉旧的 FBO/纹理,否则每次缩放泄漏 4 张纹理。
    let resizeRaf = 0
    const rebuildFBOs = () => {
      resizeRaf = 0
      if (disposed || gl.isContextLost()) return
      if (!resize()) return
      ;[simA, simB, blurH, blurV].forEach((f) => { gl.deleteFramebuffer(f.fbo); gl.deleteTexture(f.tex) })
      simA = makeFBO(); simB = makeFBO(); blurH = makeFBO(); blurV = makeFBO()
    }
    const ro = typeof ResizeObserver !== 'undefined'
      ? new ResizeObserver(() => { if (!resizeRaf) resizeRaf = requestAnimationFrame(rebuildFBOs) })
      : null
    ro?.observe(canvas)

    // ── 上下文丢失处理(P0 的另一半)──
    // 超出并发上下文硬上限时浏览器会强制丢弃最老的上下文。不处理的后果不是报错而是:canvas 静默
    // 变空白,而 RAF 照常每帧对着死上下文发一整套 GL 命令 —— 画面没了开销全在。故这里立刻停 RAF。
    // ⚠️ preventDefault() 是必须的:不调它浏览器根本不会派发 webglcontextrestored。

    // 本 effect 实例的生命周期内是否发生过丢失。cleanup 要靠它判断"资源还在不在" ——
    // 不能只看 isContextLost():恢复后它已是 false,但丢失前建的那批对象早已失效(见 cleanup 注释)。
    let contextWasLost = false
    const onLost = (e: Event) => {
      e.preventDefault()
      contextWasLost = true
      cancelAnimationFrame(raf)
      raf = 0
      if (resizeRaf) { cancelAnimationFrame(resizeRaf); resizeRaf = 0 }
    }
    const onRestored = () => {
      if (disposed) return
      if (restoreCountRef.current >= MAX_CONTEXT_RESTORES) return // 抖动保护,见常量注释
      restoreCountRef.current += 1
      setEpoch((n) => n + 1) // 重跑本 effect:cleanup 先跑,资源在新上下文上重建一遍
    }
    canvas.addEventListener('webglcontextlost', onLost)
    canvas.addEventListener('webglcontextrestored', onRestored)

    const render = (tms: number) => {
      if (disposed) return
      // 双保险:事件可能比下一帧晚到(或被别处 cancel 打乱),每帧再确认一次上下文还活着。
      // 死上下文上的 GL 调用全是静默 no-op —— 只烧 CPU/GPU,不报错,所以必须主动查。
      if (gl.isContextLost()) { raf = 0; return }
      if (typeof document !== 'undefined' && document.hidden) { raf = requestAnimationFrame(render); return }
      const elapsed = (performance.now() - startRef.current) / 1000
      const t = tms * 0.001

      gl.viewport(0, 0, canvas.width, canvas.height)
      gl.bindVertexArray(vao)

      // 强度缓动:每帧朝目标逼近(~2%/帧),配色切换平滑过渡而非硬跳。
      curIntensity += (targetIntensityRef.current - curIntensity) * 0.04
      const tri = triadForIntensity(curIntensity)

      // sim → simB(读 simA 上一帧)
      gl.bindFramebuffer(gl.FRAMEBUFFER, simB.fbo)
      gl.useProgram(simProg)
      gl.uniform1f(U.time, t); gl.uniform1f(U.slider, 1.0); gl.uniform1f(U.elapsed, elapsed)
      gl.uniform3f(U.ember, ...tri.ember); gl.uniform3f(U.glow, ...tri.glow); gl.uniform3f(U.core, ...tri.core)
      gl.activeTexture(gl.TEXTURE0); gl.bindTexture(gl.TEXTURE_2D, simA.tex); gl.uniform1i(U.back, 0)
      gl.drawArrays(gl.TRIANGLES, 0, 6)

      // blur H → blurH
      gl.useProgram(blurProg)
      gl.uniform2f(U.blurRes, canvas.width, canvas.height)
      gl.bindFramebuffer(gl.FRAMEBUFFER, blurH.fbo)
      gl.uniform2f(U.blurDir, 1, 0); gl.uniform1f(U.blurExt, 1)
      gl.bindTexture(gl.TEXTURE_2D, simB.tex); gl.uniform1i(U.blurTex, 0)
      gl.drawArrays(gl.TRIANGLES, 0, 6)
      // blur V → blurV
      gl.bindFramebuffer(gl.FRAMEBUFFER, blurV.fbo)
      gl.uniform2f(U.blurDir, 0, 1); gl.uniform1f(U.blurExt, 0)
      gl.bindTexture(gl.TEXTURE_2D, blurH.tex)
      gl.drawArrays(gl.TRIANGLES, 0, 6)

      // composite → 屏幕
      gl.bindFramebuffer(gl.FRAMEBUFFER, null)
      gl.useProgram(compProg)
      gl.activeTexture(gl.TEXTURE0); gl.bindTexture(gl.TEXTURE_2D, simB.tex); gl.uniform1i(U.compScene, 0)
      gl.activeTexture(gl.TEXTURE1); gl.bindTexture(gl.TEXTURE_2D, blurV.tex); gl.uniform1i(U.compGlow, 1)
      gl.drawArrays(gl.TRIANGLES, 0, 6)

      // 乒乓交换 simA/simB(拖尾)
      const tmp = simA; simA = simB; simB = tmp
      raf = requestAnimationFrame(render)
    }
    raf = requestAnimationFrame(render)

    return () => {
      disposed = true
      cancelAnimationFrame(raf)
      if (resizeRaf) cancelAnimationFrame(resizeRaf)
      ro?.disconnect()
      // ⚠️ 先摘监听再 loseContext():下面那句会**同步**派发 webglcontextlost,若 onLost 还挂着就会
      // preventDefault() → 浏览器随后派发 restored → onRestored 对着已卸载的组件 setEpoch。
      canvas.removeEventListener('webglcontextlost', onLost)
      canvas.removeEventListener('webglcontextrestored', onRestored)
      // 释放 WebGL 资源 + 强制丢弃上下文(彻底还上下文名额,数百号切换不泄漏)。
      // ⚠️ 两种情况都必须跳过这一整段:
      //   ① 上下文此刻是丢的 —— 那些对象早已随它失效,再 delete 只是徒劳;
      //   ② 本实例经历过丢失、现在是 restored 状态 —— isContextLost() 已回 false,但句柄仍是失效的老对象,
      //      而这里若照常 loseContext() 就会**再丢一次** → 又派发 restored → 又 bump epoch → cleanup 再丢…
      //      恰好把恢复机制变成一台 lost/restored 振荡器,3 次额度瞬间烧完、画面永久留空。
      if (!contextWasLost && !gl.isContextLost()) {
        ;[simA, simB, blurH, blurV].forEach((f) => { gl.deleteFramebuffer(f.fbo); gl.deleteTexture(f.tex) })
        gl.deleteBuffer(vbo); gl.deleteVertexArray(vao)
        gl.deleteProgram(simProg); gl.deleteProgram(blurProg); gl.deleteProgram(compProg)
        gl.getExtension('WEBGL_lose_context')?.loseContext()
      }
    }
  }, [active, epoch])

  if (!active) return null
  return (
    <canvas
      ref={canvasRef}
      className={className}
      // ⭐没有 mixBlendMode:改由 composite shader 输出预乘 alpha(见 FRAG_COMP 注释)。
      // 带 blend 的层无法被合成器缓存成独立 layer,每帧都要与背后整片背景重混一次 —— 那是实测
      // 「GPU 进程 102% / WebContent 0.0%」的直接来源。去掉后这层可以被当普通 layer 缓存。
      style={{ display: 'block', width: '100%', height: '100%', pointerEvents: 'none' }}
      aria-hidden
    />
  )
}
