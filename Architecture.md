# simple_pianoroll 架构设计

> 一个"简易钢琴窗"小工具:像 FL Studio 那样子在网格里编排音符,但支持**不同平均律**(12-EDO / 19-EDO / 24-EDO / 31-EDO 等),可以**实时播放**循环 loop,用于作曲实验;带一个简单的音色(Timbre)调节和一条单轨效果链(Effect)。
>
> 本文件描述目标架构与落地路径。当前 crate 还处于脚手架阶段(仅 `Cargo.toml` + `src/main.rs` 的 `Hello, world`)。

---

## 1. 目标与范围

### 目标 (Goals)
- 提供一个图形化 **钢琴窗编辑器**(piano roll),以网格形式编辑音符(音高 × 时间)。
- 支持**切换不同的平均律**(N-EDO),切换后:
  - 钢琴窗垂直方向"每八度分多少行"随之改变(12 律 → 12 行,19 律 → 19 行,24 律 → 24 行);
  - 发声的**音高映射**(note → 频率)同步改变。
- **实时播放**:transport 推动播放头,循环播放当前 pattern。
- 作曲实验友好:快速试听、试不同调律下的和声/旋律。
- **简单音色**:给这一轨选一个波形/振荡器,调 ADSR / 增益等几个参数。
- **简单效果**:一条单轨效果链(例如低通 + 延迟 + 混响),不做多轨。

### 非目标 (Non-Goals)
- 多轨道/复杂混音、自动化曲线、MIDI 录制、导出音频/插件。
- 高端音色引擎(SIMD、物理建模)。
- 目标是"够用来做调律实验的轻量工具",而不是完整 DAW。

---

## 2. 项目结构与依赖

`simple_pianoroll` 是 `C:\projects\dsp` 下的**独立 crate**(与 `i_am_dsp/` 平级),直接依赖 `i_am_dsp` 库复用其中已实现的 DSP 基础设施。

```
C:\projects\dsp\
├── i_am_dsp\            # 上游 DSP 库(workspace):i_am_dsp / i_am_dsp_derive / i_am_dsp_iced / i_am_plugin
├── simple_pianoroll\    # 本工具(crate)
│   ├── Cargo.toml
│   └── src\
└── ...
```

`Cargo.toml` 建议的依赖骨架(示意,待定):

```toml
[dependencies]
i_am_dsp = { path = "../i_am_dsp/i_am_dsp" }   # 复用 Tuning / Adsr / 效果器 / NoteEvent 等
egui = "..."          # UI(piano roll 用 egui painter 画自定义网格)
cpal = "..."          # 实时音频输出(同 i_am_dsp real_time_demo 的做法)
anyhow = "..."        # 错误处理
# serde / ron:用于 pattern 的保存/载入(阶段 M5)
```

> 依赖 `i_am_dsp` 时需要注意:它的实时 demo 走 `egui` + `cpal`,本工具沿用它走 `egui` 即可,不需要 `iced` 那套插件 UI。

---

## 3. 总体架构与数据流

```
┌────────────────────────────── egui 窗口 (UI 线程) ─────────────────────────────┐
│                                                                                │
│  ┌──────────────┐   ┌──────────────────────────┐   ┌─────────────────────────┐ │
│  │ 调律选择面板   │──▶│ 钢琴窗编辑器 (PianoRoll)    │   │ 音色面板 + 效果链面板    │ │
│  │ N-EDO / 纯率  │   │  网格 + 音符编辑(增删移动)  │   │  波形/ADSR/效果参数       │ │
│  └──────┬───────┘   └────────────┬─────────────┘   └───────────┬─────────────┘ │
│         │                        │                             │               │
│         ▼                        ▼                             ▼               │
│   Tuning 选择 (整数)          Pattern 数据 (音符列表)      生成器/效果参数变更      │
└────────┼─────────────────────────┼───────────────────────────────┼─────────────┘
         │   (共享状态经 Arc<Mutex<SharedState>> 交给音频线程)        │
         ▼                         ▼                               ▼
┌──────────────────────────── 音频线程 (cpal 回调) ──────────────────────────────┐
│                                                                                │
│   Transport/Sequencer                                                          │
│     - 逐样本推进 sample 计数器                                                   │
│     - 播放头跨过音符边界时,向 ctx 塞入 NoteOn/NoteOff                            │
│             │                                                                  │
│             ▼  (NoteEvent 经 ProcessContext)                                    │
│   Adsr<Osc, TuningSys>  (复音 AHD 发生器,吃 NoteEvent)                          │
│             │                                                                  │
│             ▼                                                                  │
│   单轨效果链 (低通/延迟/混响 …)  →  master → 输出                                │
└──────────────────────────────────────────────────────────────────────────────┘
```

核心思想:**"Pattern 编辑"与"运行时发声"通过 transport 桥接**——编辑阶段改动的是稳定的 `Pattern` 数据结构;播放阶段由 transport 按时间轴把 `Pattern` 转成 `NoteEvent` 流喂给现有的复音 `Adsr` 发生器。UI 与音频线程之间用 `Arc<Mutex<...>>` 共享状态(沿用 `i_am_dsp` 的 `real_time_demo`/`DspDemo` 的做法)。

---

## 4. 核心模块

### 4.1 调律系统 (Tuning)
**直接复用 `i_am_dsp` 现成的 `Tuning` trait**(`i_am_dsp/src/generators/adsr.rs`):

| 类型 | 说明 |
|---|---|
| `EqualTemperament` | 12-EDO(现成) |
| `NEdoTuning::<N>` | **N 等分八度** —— 19 / 24 / 31 / 53 等微音程平均律 |
| `JustIntonation` | 五限纯率 |
| `PythagoreanTuning` | 毕达哥拉斯律 |
| `ScaleTuning<N>` + 预设 (`PENTATONIC` …) | 任意固定音阶 |

`Tuning::get_frequency(&self, note: f32)` 接受 **f32 音高**,天然支持微音程/小数音高(后续可做滑音)。

**需要注意的两个改动/约定:**

1. **运行时切换**:`Adsr` 目前把 `TuningSys` 作为**泛型类型参数**(`Adsr<Osc, TuningSys, CHANNELS>`),类型在编译期固定,无法在下拉菜单里切换 12↔19↔24。需要一个轻量重构,两种可选路径:
   - 路径 A(推荐,改动小):新增一个运行时枚举 `TuningKind`(例如 `Nedo(u32) | Just | Pythagorean | Custom`),并为它实现 `Tuning`;`Adsr` 里用 `Box<dyn Tuning>`(或直接给本 crate 包一层只持有 `Box<dyn Tuning>` 的包装发生器)。
   - 路径 B:`simple_pianoroll` 内部做一个 `DynAdsr` 包装,内部持有 `Box<dyn Generator>` 并按所选调律把 pattern 音高换算成频率后再驱动。这样不动上游 `Adsr`。
   - 无论哪条路,**核心是"调律选择"同时驱动两处**:钢琴窗 Y 轴的行数/排布,和发声时的 note→频率。

2. **音高编号约定**:库内音高编号并非标准 MIDI(`A4_MIDI = 57`,`C4_MIDI = 48`,即 C4=48)。`simple_pianoroll` 应定义**自己的一套统一编号**:让一个整数表示"所选调律下、以某个基准(如 C4)为起点的第 k 个音级",并用同一个编号同时索引网格行与 `get_frequency`。切换 N-EDO 后编号语义随 N 变化(见 §5)。

### 4.2 钢琴窗编辑器 (PianoRollEditor)
这是**全新实现**的部分,需要自己用 `egui` 的 `Painter` + 交互绘制:

- **网格映射**:
  - Y 轴:按 `N 步/八度` 分行(垂直方向),每八度 N 行;整数行 = 一个音级(可编辑的音符落在整数行上)。
  - X 轴:按 step 分列,建议默认 16 分音符分辨率,支持整列宽度 == 一个 step。
- **视觉**:突出"白键/根音"行(相对主音为纯律级数的行),区分音级颜色,绘制小节线、刻度。
- **交互**:
  - 左键点击空位 → 添加音符(默认一拍长);
  - 左键拖拽 → 连续绘制一段 run;
  - 拖拽音符本体 → 移动(移音高/移时间);
  - 拖拽音符右边缘 → 改长度;
  - 右键点击音符 → 删除;
  - 播放头当前列高亮。
- **数据模型**:维护 `Vec<Note>`,其中 `Note { start_step: usize, length_steps: usize, pitch: i32, velocity: f32 }`。Y 轴行高与 `pitch` 一一对应(行号 == pitch)。

### 4.3 播放编排 (Transport / Sequencer)
把 `Pattern`(音符列表)变成 `NoteEvent` 流:

- **时钟**:音频回调里维护一个绝对值 `sample_counter`(或 `(bar, beat, step, sample)`)。
- **事件生成**:每次回调(一个缓冲 block)内,计算 `[prev_sample, cur_sample)` 区间里 "有音符开始" 的位置,产生 `NoteEvent::NoteOn{ time, channel, note, velocity }`;音符结束位置产生 `NoteEvent::NoteOff`。
- **循环**:pattern 有固定长度(以 step 计),`sample_counter % pattern_len_samples` 得到当前播放位置,到末尾自动回到开头 → 循环播放。
- **播放控制**:`playing` 布尔、`tempo(BPM)`、`steps_per_beat`、`resolution(steps_per_quarter)`。暂停时静音并可以 `ImmediateStop` 停掉所有在响音符(复用 `NoteEvent::ImmediateStop`)。
- **注入点**:把生成的事件放进 `SimpleContext::midi_events`(或等价 `ProcessContext`),`Adsr` 在 `process`/`generate` 时通过 `next_event()` 消费并触发复音音符。事件在 block 粒度对齐(缓冲开头批量处理)即可满足轻量使用。

> 现有 `ProcessContext` / `NoteEvent` 已支持这一切;`DspDemo::SharedData::generate` 就是"生成器 + 轨 bus + 效果链"的参考样例,`simple_pianoroll` 可裁剪成单轨版本。

### 4.4 音色面板 (Timbre)
- 复用 `real_time_demo::GENERATOR_LIST` 里的现成发声器(波表 / 加法 / 锯齿 / 拨弦等)或直接选 `Adsr` + 一种波形。
- UI 暴露少量关键参数:波形选择、`Adsr` 的 attack / hold / decay / sustain / release、gain。这些参数直接映射到 `Adsr` 字段(`#[derive(Parameters)]` 已生成 `Parameter` 体系,可复用 `get_parameters` / `set_parameter`)。

### 4.5 效果链 (Effects, 单轨)
- 复用 `real_time_demo::EFFECT_LIST` 现成效果器(低通、延迟、混响、失真、压缩…)。
- 以 `Vec<Box<dyn Effect>>` 顺序应用在该轨输出 bus 上(复用库内置的 `Vec<T>: Effect` 实现,天然支持链式 `process`),每个效果带一个 `mix` 湿/干比例。

### 4.6 音频引擎 (Audio Engine)
- 沿用 `i_am_dsp` 的 `real_time_demo` 模式:`cpal::build_output_stream` 打开默认输出设备,每次缓冲在回调里逐样本调用 `SharedData::generate(&mut ctx)`。
- `SharedState`(pattern、调律编号、生成器/效果参数、transport 状态)放在 `Arc<Mutex<...>>`,UI 线程改、音频线程读,回调里短暂加锁(与 `DspDemo` 相同)。

---

## 5. 不同平均律如何落地(关键设计)

目标是"切换平均律后既能听到、又能看到八度被重新划分"。核心是**统一音高编号**,同时驱动网格与频率:

1. **统一编号**:定义 `pitch_index`(整数)表示"从基准 C(如 C4)起的第几个音级",步长 = 一个 EDO 度。
2. **网格**:Y 轴每八度显示 N 行,`pitch_index` 直接对应行号;第 `k` 个八度内行号范围 `[k*N, (k+1)*N)`。
3. **频率**:`freq = tuning.get_frequency(c4_base_index + pitch_index as f32)`,其中:
   - 对 `NEdoTuning::<N>`:`get_frequency` 用 `(note - A4_MIDI)/N` 的指数,天然把每 N 个整数映射一个八度;
   - 对 12-EDO/纯率/毕氏,网格保持 12 行/八度,只有频率关系不同。
4. **切换行为**:
   - 整数音高采用"度"语义,切到不同 N 后同一行在频率上属于不同音级——这正是"调律实验"的观察对象;
   - 切换时保留音符的 `pitch_index`,重新按新调律算频率(而不是按绝对频率反算),保证旋律轮廓直观。

---

## 6. UI 布局草图

```
┌────────────────────────────────────────────────────────────────┐
│ [调律: 12-EDO ▾] [19-EDO] [24-EDO] [纯率]   BPM[120]  ▶/⏸/⏹   │
├──────────┬─────────────────────────────────────────────────────┤
│ 钢琴键列  │           钢琴窗网格 (X: steps, Y: pitch)            │
│ (可点击试听│   ┌─┬─┬─┬─┬─┬─┬─┬─┬─┬─┬─┬─┬─┬─┬─┬─┐                │
│  音高)   │   │ │▮│ │ │▮│ │ │ │▮│ │ │▮│ │ │ │ │  ← 音符块       │
│          │   └─┴─┴─┴─┴─┴─┴─┴─┴─┴─┴─┴─┴─┴─┴─┘                │
├──────────┴─────────────────────────────────────────────────────┤
│ 音色: 波形[▾] Attack[——] Decay[——] ...                        │
│ 效果链: [低通: freq] [延迟: time/mix] [混响: mix]              │
└────────────────────────────────────────────────────────────────┘
```

---

## 7. 里程碑 (Milestones)

- **M1 脚手架 + 出声链路**:`simple_pianoroll` 搭出 `egui` 窗口 + `cpal` 音频回调 + `i_am_dsp` 的 `Adsr` 发声;不画网格,先能实时放一段固定旋律/音符列表,证明"pattern → NoteEvent → Adsr → 声卡"链路通。
- **M2 调律切换**:加 `Box<dyn Tuning>` / `TuningKind` 运行时切换,下拉在 12 / 19 / 24 … 之间切换,验证实际音高按比率变化。
- **M3 钢琴窗 + transport**:画出可编辑网格、音符增删移动、播放头、循环播放 —— 到这个阶段就是一个能"作曲实验"的雏形。
- **M4 音色 + 效果面板**:接回 `GENERATOR_LIST` / `EFFECT_LIST`,加波形/ADSR、效果链参数。
- **M5 打磨**:pattern 保存/载入(serde/ron)、节拍器、量化、音符按音级着色、主音/根音可设。

---

## 8. 关键决策记录 (Decision Log)

1. **在 `i_am_dsp` 之外新建独立 crate**(而非改 `i_am_dsp` 内部 demo):保持上游库干净,本工具可独立迭代。
2. **UI 用 `egui`**(而非 iced):与 `i_am_dsp` 实时 demo 同栈,画自定义网格方便,桌面 standalone 更简单。
3. **复用而非重造发声**:节奏/作曲落到已有的复音 `Adsr` + `NoteEvent` 机制,把新增工作量集中在"编辑器 + transport"。
4. **调律统一编号**,同时服务网格与频率,是"不同平均律"需求的核心抽象。
5. **单轨 + 有限参数**:刻意收敛复杂度,保证能实验调律而不引入 DAW 级别复杂度。

---

## 9. 开放问题 (Open Questions)

- 上游 `Adsr` 的 tuning 由**类型参数**承载,是否值得向上游提一个 `Box<dyn Tuning>` 的小 PR,还是本 crate 内部包装解决?(倾向先在 crate 内解决,保持上游不动)
- 切调律时 `pitch_index` 语义变化,是否需要在 UI 上提示"同一行在不同律下的实际音级"?
- 是否需要支持非等分调律(纯率/毕氏)下的自定义主音/移调?(列为 M5 之后)
