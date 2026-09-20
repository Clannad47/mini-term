//! shell 图标:按 pane 的 shell 名画出「这一格跑的是哪个壳」。
//!
//! # 为什么要有它
//!
//! 终端 tab 上原先一律画 [`StatusDot`](super::status::StatusDot),而 `idle` 态
//! 那颗是**空心圈**:一屏五个闲置终端就是五颗一模一样的 ○,占着位置却不带任何
//! 信息。闲置时改画 shell 图标,同一个 16×16 槽位立刻变成「pwsh / cmd / bash」
//! 的速读标识;真出状态(AI 在跑 / 出错 / 等回应)时再让位给状态灯 ——
//! **有事看灯,没事看壳**。
//!
//! # 分辨靠什么
//!
//! 14px 上笔画细节是读不出来的,能读的只有**剪影 + 色相**两层:
//!
//! | 剪影 | 谁 | 色相 |
//! |---|---|---|
//! | 圆角实心块 + 白「›_」 | pwsh | 亮蓝 |
//! | 圆角实心块 + 白「›_」 | Windows PowerShell | 深蓝 |
//! | **直角**实心块 + 白「›_」 | cmd | 中灰 |
//! | 描边框 + 「$_」 | bash | 绿 |
//! | 描边框 + 「%_」 | zsh | 紫 |
//! | 小鱼 | fish | 青 |
//! | **无框**的粗「> _」 | nu | 亮绿 |
//! | 圆环 + 三点 | WSL | Ubuntu 橙 |
//! | 描边框 + 「›_」(跟随主题色) | 未知 | 主题 |
//!
//! 实心 / 描边 / 无框 / 圆形是四档互不相同的剪影;同剪影的三家(pwsh / WinPS /
//! cmd)靠蓝—深蓝—灰拉开,cmd 另外再用直角与圆角区分。
//!
//! 图形是**自绘**的示意标记,不是官方 logo —— 理由与整个 [`super`] 模块一致
//! (见 [`super::vector`] 的模块注释),且 shell 这几家本来也没有统一的矢量标识。
//!
//! # 宿主接线(mt-app)
//!
//! ```ignore
//! use mt_ui::icons::{ShellIcon, ShellKind};
//!
//! if pane.status == PaneStatus::Idle {
//!     ShellIcon::new(ShellKind::from_shell_name(&pane.shell_name)).size(px(14.0))
//! } else {
//!     ui::status_dot(pane.status)
//! }
//! ```
//!
//! 两个分支**必须包在同一个固定尺寸的盒子里**(16×16 居中),否则状态一变
//! tab 内容就横跳一格。

use gpui::{App, Hsla, IntoElement, Pixels, RenderOnce, Window, px};

use super::vector::{Geom, Ink, Shape, VectorIcon};

/// 一种 shell。够用即可 —— 认不出来的一律 [`ShellKind::Unknown`],画通用终端框。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ShellKind {
    /// PowerShell 7+(`pwsh`)
    Pwsh,
    /// 随 Windows 出厂的 5.1(`powershell`)
    WindowsPowerShell,
    Cmd,
    Bash,
    Zsh,
    Fish,
    /// nushell
    Nu,
    /// WSL 发行版(Ubuntu / Debian / Arch / Alpine …)
    Wsl,
    #[default]
    Unknown,
}

/// 全部取值(遍历 / 单测用)。
pub const ALL_SHELL_KINDS: &[ShellKind] = &[
    ShellKind::Pwsh,
    ShellKind::WindowsPowerShell,
    ShellKind::Cmd,
    ShellKind::Bash,
    ShellKind::Zsh,
    ShellKind::Fish,
    ShellKind::Nu,
    ShellKind::Wsl,
    ShellKind::Unknown,
];

impl ShellKind {
    /// 稳定的字符串名(调试 / 单测用,不进 i18n)。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pwsh => "pwsh",
            Self::WindowsPowerShell => "powershell",
            Self::Cmd => "cmd",
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Fish => "fish",
            Self::Nu => "nu",
            Self::Wsl => "wsl",
            Self::Unknown => "unknown",
        }
    }

    /// 从 shell 名(可能是裸名、也可能是整条可执行文件路径)认出是哪一家。
    ///
    /// 规则是**大小写不敏感的子串匹配**,`RULES` 的顺序即优先级。
    ///
    /// ⚠️ 顺序不是随便排的,两处坑:
    /// - `powershell` 与 `pwsh` 互不包含,但 `WindowsPowerShell\v1.0\powershell.exe`
    ///   这类整路径两条都要能落到「深蓝」那一档,所以 `pwsh` 在前、`powershell` 在后;
    /// - **`nu` 必须排在最后** —— 它只有两个字母,`gnu bash` 里就有一个。bash /
    ///   wsl 那几条先把该认的认走,剩下的才轮到它。
    pub fn from_shell_name(name: &str) -> Self {
        /// (这一家, 命中它的子串们)。顺序即优先级。
        const RULES: &[(ShellKind, &[&str])] = &[
            (ShellKind::Pwsh, &["pwsh"]),
            (ShellKind::WindowsPowerShell, &["powershell"]),
            (ShellKind::Cmd, &["cmd"]),
            // `git bash` 是 Windows 上最常见的写法,与裸 `bash` 同一档
            (ShellKind::Bash, &["git bash", "bash"]),
            (ShellKind::Zsh, &["zsh"]),
            (ShellKind::Fish, &["fish"]),
            (
                ShellKind::Wsl,
                &["wsl", "ubuntu", "debian", "arch", "alpine"],
            ),
            // ⚠️ 只有两个字母,必须垫底(见函数注释)
            (ShellKind::Nu, &["nushell", "nu"]),
        ];

        let hay = name.to_ascii_lowercase();
        for (kind, needles) in RULES {
            if needles.iter().any(|n| hay.contains(n)) {
                return *kind;
            }
        }
        Self::Unknown
    }

    fn shapes(self) -> &'static [Shape] {
        match self {
            Self::Pwsh => PWSH,
            Self::WindowsPowerShell => WINDOWS_POWERSHELL,
            Self::Cmd => CMD,
            Self::Bash => BASH,
            Self::Zsh => ZSH,
            Self::Fish => FISH,
            Self::Nu => NU,
            Self::Wsl => WSL,
            Self::Unknown => UNKNOWN,
        }
    }
}

// ───────────────────────── 配色 ─────────────────────────
//
// 「品牌色倾向」而非官方色值:暗底上要看得清,所以深色那几家(Windows
// PowerShell 的藏青 #012456、cmd 的近黑)都提亮到仍能与邻居拉开的程度 ——
// 原样照搬会在 #1c1a18 的底上糊成一团黑。

/// pwsh:亮蓝。
const PWSH_INK: Ink = Ink::Rgb(0x33, 0x86, 0xe8);
/// Windows PowerShell:同色系压深一档(官方藏青提亮后的落点)。
const WINPS_INK: Ink = Ink::Rgb(0x1e, 0x4f, 0x9c);
/// cmd:中灰(官方近黑提亮)。
const CMD_INK: Ink = Ink::Rgb(0x6e, 0x6e, 0x6e);
/// bash:GNU Bash 绿。
const BASH_INK: Ink = Ink::Rgb(0x4e, 0xaa, 0x25);
/// zsh:紫(与 bash 绿拉开,oh-my-zsh 一脉的色感)。
const ZSH_INK: Ink = Ink::Rgb(0xa6, 0x7d, 0xe0);
/// fish:青。
const FISH_INK: Ink = Ink::Rgb(0x3a, 0xb5, 0xc9);
/// nu:亮绿。
const NU_INK: Ink = Ink::Rgb(0x3f, 0xb9, 0x50);
/// WSL:Ubuntu 橙。
const WSL_INK: Ink = Ink::Rgb(0xe9, 0x54, 0x20);
/// 实心块上的字形色。**不用 [`Ink::Contrast`]** —— 那一档取的是宿主面板底色
/// (暗底主题下是深色),画在品牌蓝块上会变成「深色凿空」,与 PowerShell
/// 官方那个白色提示符正相反。
const GLYPH_INK: Ink = Ink::Rgb(0xff, 0xff, 0xff);

// ───────────────────────── 几何 ─────────────────────────

/// 实心块的方位(pwsh / WinPS / cmd 共用),圆角各自给。
const BLOCK_X: f32 = 0.05;
const BLOCK_Y: f32 = 0.15;
const BLOCK_W: f32 = 0.90;
const BLOCK_H: f32 = 0.70;

/// PowerShell 一家的圆角;cmd 用 [`BLOCK_ROUND_SHARP`] 的直角剪影区分。
const BLOCK_ROUND: f32 = 0.18;
const BLOCK_ROUND_SHARP: f32 = 0.04;

/// 描边框(bash / zsh / 未知):同一扇窗,只画轮廓。
const FRAME: Geom = Geom::Rect {
    x: 0.06,
    y: 0.16,
    w: 0.88,
    h: 0.68,
    round: 0.14,
};
const FRAME_W: f32 = 0.09;

/// 框内字形的笔宽。14px 上约 1.4px —— 再细会被 [`super::vector`] 的 0.5px
/// 下限吃掉细节,再粗两笔就糊在一起。
const GLYPH_W: f32 = 0.10;

/// 提示符的尖括号「›」。
const CHEVRON: Geom = Geom::Polyline(&[(0.26, 0.36), (0.42, 0.50), (0.26, 0.64)]);
/// 提示符后的光标「_」。
const CARET: Geom = Geom::Polyline(&[(0.50, 0.66), (0.74, 0.66)]);

const fn block(ink: Ink, round: f32) -> Shape {
    Shape::fill(
        ink,
        Geom::Rect {
            x: BLOCK_X,
            y: BLOCK_Y,
            w: BLOCK_W,
            h: BLOCK_H,
            round,
        },
    )
}

/// pwsh:亮蓝圆角块 + 白「›_」。
const PWSH: &[Shape] = &[
    block(PWSH_INK, BLOCK_ROUND),
    Shape::line(GLYPH_INK, GLYPH_W, CHEVRON),
    Shape::line(GLYPH_INK, GLYPH_W, CARET),
];

/// Windows PowerShell:同形深一档的蓝。
const WINDOWS_POWERSHELL: &[Shape] = &[
    block(WINPS_INK, BLOCK_ROUND),
    Shape::line(GLYPH_INK, GLYPH_W, CHEVRON),
    Shape::line(GLYPH_INK, GLYPH_W, CARET),
];

/// cmd:深灰**直角**块 + 白「>_」。直角是与 PowerShell 一家的剪影区分。
const CMD: &[Shape] = &[
    block(CMD_INK, BLOCK_ROUND_SHARP),
    Shape::line(GLYPH_INK, GLYPH_W, CHEVRON),
    Shape::line(GLYPH_INK, GLYPH_W, CARET),
];

/// bash:绿色终端框 + 「$_」。`$` = 上下两段圆弧(S)+ 一根竖笔。
const BASH: &[Shape] = &[
    Shape::line(BASH_INK, FRAME_W, FRAME),
    // 上半弯:从右下起,逆时针绕过顶,收到左腰
    Shape::line(
        BASH_INK,
        0.085,
        Geom::Arc {
            c: (0.345, 0.40),
            r: 0.075,
            from: 45.0,
            sweep: -250.0,
        },
    ),
    // 下半弯:从左腰起,顺时针绕过右与底,收到左下
    Shape::line(
        BASH_INK,
        0.085,
        Geom::Arc {
            c: (0.345, 0.545),
            r: 0.075,
            from: -155.0,
            sweep: 285.0,
        },
    ),
    Shape::line(
        BASH_INK,
        0.06,
        Geom::Polyline(&[(0.345, 0.28), (0.345, 0.67)]),
    ),
    Shape::line(
        BASH_INK,
        GLYPH_W,
        Geom::Polyline(&[(0.52, 0.66), (0.76, 0.66)]),
    ),
];

/// zsh:紫色终端框 + 「%_」。`%` = 一道斜杠 + 两颗实心点。
const ZSH: &[Shape] = &[
    Shape::line(ZSH_INK, FRAME_W, FRAME),
    Shape::line(ZSH_INK, 0.08, Geom::Polyline(&[(0.29, 0.66), (0.47, 0.33)])),
    Shape::fill(
        ZSH_INK,
        Geom::Circle {
            c: (0.30, 0.38),
            r: 0.065,
        },
    ),
    Shape::fill(
        ZSH_INK,
        Geom::Circle {
            c: (0.46, 0.61),
            r: 0.065,
        },
    ),
    Shape::line(
        ZSH_INK,
        GLYPH_W,
        Geom::Polyline(&[(0.56, 0.66), (0.78, 0.66)]),
    ),
];

/// fish:一条朝右游的简笔小鱼(鱼身 + 尾鳍 + 眼)。
///
/// 这是全表唯一不带方框的**有机形**,14px 上一眼就能从一堆方块里挑出来。
const FISH: &[Shape] = &[
    // 鱼身:左尖(接尾)右尖(鼻)的梭形
    Shape::fill(
        FISH_INK,
        Geom::Polygon(&[
            (0.88, 0.50),
            (0.70, 0.30),
            (0.48, 0.30),
            (0.32, 0.50),
            (0.48, 0.70),
            (0.70, 0.70),
        ]),
    ),
    // 尾鳍:中间凹进去一块的燕尾
    Shape::fill(
        FISH_INK,
        Geom::Polygon(&[(0.32, 0.50), (0.12, 0.28), (0.20, 0.50), (0.12, 0.72)]),
    ),
    // 眼睛是**挖空**语义,用宿主给的 contrast(面板底色)
    Shape::fill(
        Ink::Contrast,
        Geom::Circle {
            c: (0.72, 0.45),
            r: 0.055,
        },
    ),
];

/// nu:不画框,只剩一个特别粗的绿色「> _」。
const NU: &[Shape] = &[
    Shape::line(
        NU_INK,
        0.13,
        Geom::Polyline(&[(0.24, 0.22), (0.56, 0.48), (0.24, 0.74)]),
    ),
    Shape::line(NU_INK, 0.13, Geom::Polyline(&[(0.62, 0.74), (0.86, 0.74)])),
];

/// WSL:橙色圆环 + 环上三颗点(Ubuntu 的「朋友圈」味道,不是官方 logo)。
const WSL: &[Shape] = &[
    Shape::line(
        WSL_INK,
        0.10,
        Geom::Circle {
            c: (0.5, 0.5),
            r: 0.34,
        },
    ),
    Shape::fill(
        WSL_INK,
        Geom::Circle {
            c: (0.5, 0.16),
            r: 0.095,
        },
    ),
    Shape::fill(
        WSL_INK,
        Geom::Circle {
            c: (0.794, 0.67),
            r: 0.095,
        },
    ),
    Shape::fill(
        WSL_INK,
        Geom::Circle {
            c: (0.206, 0.67),
            r: 0.095,
        },
    ),
];

/// 未知:通用终端框。**跟随主题色**(`Ink::Current`)—— 认不出牌子就别编一个。
const UNKNOWN: &[Shape] = &[
    Shape::line(Ink::Current, FRAME_W, FRAME),
    Shape::line(Ink::Current, GLYPH_W, CHEVRON),
    Shape::line(Ink::Current, GLYPH_W, CARET),
];

/// 所有形状表(单测遍历用)。
#[cfg(test)]
pub(super) fn shape_tables() -> Vec<&'static [Shape]> {
    ALL_SHELL_KINDS.iter().map(|k| k.shapes()).collect()
}

/// shell 图标。
///
/// ```ignore
/// ShellIcon::new(ShellKind::from_shell_name(&pane.shell_name)).size(px(14.0))
/// ```
#[derive(IntoElement)]
pub struct ShellIcon {
    kind: ShellKind,
    size: Pixels,
    /// `Ink::Current` 的取色 —— 只有 [`ShellKind::Unknown`] 那枚跟着它走。
    color: Option<Hsla>,
    /// 挖空色(fish 的眼睛)。不给就用 `VectorIcon` 的默认面板底色。
    contrast: Option<Hsla>,
}

impl ShellIcon {
    /// 默认 14px —— 与 tab 上 16×16 图标槽的内容尺寸一致。
    pub fn new(kind: ShellKind) -> Self {
        Self {
            kind,
            size: px(14.0),
            color: None,
            contrast: None,
        }
    }

    pub fn size(mut self, size: Pixels) -> Self {
        self.size = size;
        self
    }

    pub fn color(mut self, color: Hsla) -> Self {
        self.color = Some(color);
        self
    }

    pub fn contrast(mut self, color: Hsla) -> Self {
        self.contrast = Some(color);
        self
    }
}

impl RenderOnce for ShellIcon {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let mut icon = VectorIcon::new(self.kind.shapes(), self.size);
        if let Some(c) = self.color {
            icon = icon.ink(c);
        }
        if let Some(c) = self.contrast {
            icon = icon.contrast(c);
        }
        icon
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 名字识别覆盖常见写法() {
        let cases: &[(&str, ShellKind)] = &[
            ("pwsh", ShellKind::Pwsh),
            ("pwsh.exe", ShellKind::Pwsh),
            (r"C:\Program Files\PowerShell\7\pwsh.exe", ShellKind::Pwsh),
            ("PowerShell", ShellKind::WindowsPowerShell),
            (
                r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
                ShellKind::WindowsPowerShell,
            ),
            ("cmd", ShellKind::Cmd),
            (r"C:\Windows\System32\cmd.exe", ShellKind::Cmd),
            ("Git Bash", ShellKind::Bash),
            ("bash", ShellKind::Bash),
            ("/usr/bin/zsh", ShellKind::Zsh),
            ("fish", ShellKind::Fish),
            ("nu", ShellKind::Nu),
            ("nushell", ShellKind::Nu),
            ("WSL", ShellKind::Wsl),
            ("Ubuntu-22.04", ShellKind::Wsl),
            ("Debian", ShellKind::Wsl),
            ("Alpine", ShellKind::Wsl),
            ("Arch", ShellKind::Wsl),
            ("", ShellKind::Unknown),
            ("elvish", ShellKind::Unknown),
        ];
        for (name, want) in cases {
            assert_eq!(
                ShellKind::from_shell_name(name),
                *want,
                "{name} 应该认成 {}",
                want.as_str()
            );
        }
    }

    #[test]
    fn 大小写不敏感() {
        for name in ["PWSH", "Pwsh", "pWsH"] {
            assert_eq!(ShellKind::from_shell_name(name), ShellKind::Pwsh);
        }
        assert_eq!(ShellKind::from_shell_name("ZSH"), ShellKind::Zsh);
    }

    #[test]
    fn nu_垫底_不抢走含_nu_两字母的名字() {
        // 「gnu bash」里就有一个 `nu`:规则表排序一旦被人按字母序整理过,
        // 所有 bash 终端会集体变成 nushell 图标
        assert_eq!(ShellKind::from_shell_name("GNU Bash"), ShellKind::Bash);
        assert_eq!(ShellKind::from_shell_name("gnu bash"), ShellKind::Bash);
        // Ubuntu 里没有连着的 `nu`(u-b-u-n-t-u),但仍按 wsl 先判一道更稳
        assert_eq!(ShellKind::from_shell_name("Ubuntu"), ShellKind::Wsl);
    }

    #[test]
    fn 每枚图标的笔画数都在可读范围内() {
        for kind in ALL_SHELL_KINDS {
            let n = kind.shapes().len();
            assert!(
                (1..=8).contains(&n),
                "{} 有 {n} 笔 —— 14px 上超过 8 笔必糊",
                kind.as_str()
            );
        }
    }

    #[test]
    fn 九家的主色互不相同() {
        // 同剪影的 pwsh / WinPS / cmd 全靠色相区分,撞色等于三家看起来一样
        let inks: Vec<Ink> = ALL_SHELL_KINDS.iter().map(|k| k.shapes()[0].ink).collect();
        for (i, a) in inks.iter().enumerate() {
            for (j, b) in inks.iter().enumerate().skip(i + 1) {
                assert_ne!(
                    a,
                    b,
                    "{} 与 {} 主色撞了",
                    ALL_SHELL_KINDS[i].as_str(),
                    ALL_SHELL_KINDS[j].as_str()
                );
            }
        }
    }

    #[test]
    fn 只有未知那枚跟随主题色() {
        // 其余八家写死品牌色:被宿主的 text_muted 染掉就失去了「一眼认牌子」的作用
        for kind in ALL_SHELL_KINDS {
            let follows_theme = kind
                .shapes()
                .iter()
                .any(|s| matches!(s.ink, Ink::Current | Ink::CurrentAlpha(_)));
            assert_eq!(
                follows_theme,
                *kind == ShellKind::Unknown,
                "{} 的跟随主题与否判错了",
                kind.as_str()
            );
        }
    }

    #[test]
    fn 剪影分四档_不是只换颜色() {
        use super::super::vector::Pen;
        // 实心块打头的是 PowerShell 一家与 cmd
        for kind in [
            ShellKind::Pwsh,
            ShellKind::WindowsPowerShell,
            ShellKind::Cmd,
        ] {
            assert!(
                matches!(kind.shapes()[0].pen, Pen::Fill),
                "{} 该是实心块",
                kind.as_str()
            );
        }
        // 描边框打头的是 bash / zsh / 未知
        for kind in [ShellKind::Bash, ShellKind::Zsh, ShellKind::Unknown] {
            assert!(
                matches!(kind.shapes()[0].pen, Pen::Line(_)),
                "{} 该是描边框",
                kind.as_str()
            );
        }
        // cmd 与 PowerShell 一家的圆角必须真的不同,否则同剪影只剩灰蓝之别
        let round_of = |kind: ShellKind| match kind.shapes()[0].geom {
            Geom::Rect { round, .. } => round,
            _ => panic!("{} 的第一笔应该是矩形", kind.as_str()),
        };
        assert!(round_of(ShellKind::Cmd) < round_of(ShellKind::Pwsh));
        // nu 不画框:第一笔是折线
        assert!(matches!(NU[0].geom, Geom::Polyline(_)));
        // WSL 是圆的
        assert!(matches!(WSL[0].geom, Geom::Circle { .. }));
    }
}
