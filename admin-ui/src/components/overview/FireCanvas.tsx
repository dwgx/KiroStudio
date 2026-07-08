import { useEffect, useRef } from 'react'

/**
 * FireCanvas —— 移植自 claude-ultracode-effort-card 的 WebGL2 火焰(原版 shader,丝滑不打折)。
 *
 * ⭐架构关键(数百号不崩):本组件**只在"火力全开"的号条件挂载**(见 StatusBars:仅 rpmSaturated
 * 的号渲染它)。因为同时饱和的号通常就 1-2 个,所以浏览器里永远只有 1-2 个 WebGL 上下文,
 * 远低于上下文硬上限(~16)。号一旦不再饱和,组件卸载 → WebGL 上下文销毁释放 → 零常驻开销。
 *
 * 三段管线(与 effort card 一致):sim(火焰模拟,读回上一帧做拖尾衰减)→ 高斯 blur → composite(辉光合成)。
 * 火焰沿条形轨道从右向左燃烧铺满(slider 恒 1.0 = 满档火力)。页面隐藏时暂停 RAF 省电。
 * WebGL2 不可用则自动降级(什么都不渲染,不报错;StatusBars 下方仍有 CSS 兜底高亮)。
 */

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

const FRAG_COMP = `#version 300 es
precision highp float;
in vec2 v_uv; out vec4 fc;
uniform sampler2D u_scene, u_glow;
void main(){
  vec3 s=texture(u_scene,v_uv).rgb;
  vec3 g=texture(u_glow,v_uv).rgb;
  fc=vec4(1.0-exp(-(s+g*1.2+s*g*0.35)*1.15),1.0);
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

export function FireCanvas({ active, intensity = 1, className }: FireCanvasProps) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null)
  const startRef = useRef<number>(0)
  // 目标强度放 ref,render 循环每帧朝它平滑逼近(颜色切换过渡动画),intensity prop 变化不重建 WebGL。
  const targetIntensityRef = useRef<number>(intensity)
  targetIntensityRef.current = intensity

  useEffect(() => {
    const canvas = canvasRef.current
    if (!canvas || !active) return
    const gl = canvas.getContext('webgl2', { preserveDrawingBuffer: false, antialias: false, alpha: true })
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

    const resize = () => {
      const rect = canvas.getBoundingClientRect()
      const dpr = Math.min(window.devicePixelRatio || 1, 2)
      canvas.width = Math.max(1, Math.round(rect.width * dpr))
      canvas.height = Math.max(1, Math.round(rect.height * dpr))
    }
    resize()

    let simA = makeFBO(), simB = makeFBO(), blurH = makeFBO(), blurV = makeFBO()

    const render = (tms: number) => {
      if (disposed) return
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
      // 释放 WebGL 资源 + 强制丢弃上下文(彻底还上下文名额,数百号切换不泄漏)
      ;[simA, simB, blurH, blurV].forEach((f) => { gl.deleteFramebuffer(f.fbo); gl.deleteTexture(f.tex) })
      gl.deleteBuffer(vbo); gl.deleteVertexArray(vao)
      gl.deleteProgram(simProg); gl.deleteProgram(blurProg); gl.deleteProgram(compProg)
      gl.getExtension('WEBGL_lose_context')?.loseContext()
    }
  }, [active])

  if (!active) return null
  return (
    <canvas
      ref={canvasRef}
      className={className}
      style={{ display: 'block', width: '100%', height: '100%', mixBlendMode: 'screen', pointerEvents: 'none' }}
      aria-hidden
    />
  )
}
