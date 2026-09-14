//! 列宽覆盖:字位簇(grapheme cluster)整簇落一格,①②③ 这类 ambiguous 符号按 2 列。
//!
//! alacritty_terminal 的 `Term::input` 是**逐字符**定列宽的(`unicode_width::width`):
//! ambiguous 一律 1 列;0 宽字符贴到前一格;宽字符各占一格。这套口径在两类地方
//! 与字体画出来的东西对不上:
//!
//! 1. **东亚宽度 A 类符号**——①②③ 这类带圈数字在 CJK 字体里是全角字形,塞进一列
//!    就压住下一个字符。
//! 2. **emoji 序列**——`❤️`(❤ + VS16)按字符算是 1 + 0 = 1 列,字形却是 2 列的
//!    彩色 emoji;`👍🏽`(👍 + 肤色修饰符)按字符算是 2 + 2 = 4 列、画成两个字形;
//!    `👨‍👩‍👧`(ZWJ 序列)被拆成三个各占 2 列的格子;`🇨🇳`(两个区域指示符)拆成
//!    两格各画半面旗。
//!
//! 这里在 VT 解析器与 `Term` 之间隔一层 [`WidthOverride`],只拦 `input`,其余方法
//! 原样转发。两条规则:
//!
//! - **簇不拆格**:来一个字符先看它按 UAX #29 是否延续光标前那一格的字位簇
//!   (VS16 / ZWJ 后的 emoji / 肤色修饰符 / 第二个区域指示符 / 组合符号……),
//!   是就以 0 宽字符的身份塞进那一格;整簇宽度(`unicode_width` 的字符串口径,
//!   已认得 emoji 呈现序列 / 修饰序列 / ZWJ 序列)从 1 变 2 时把那一格**就地拓宽**。
//! - **A 类符号按宽**:命中 [`forced_wide`] 的字符改按 2 列落格。
//!
//! 这两条与 ConPTY 1.22+ 的字位簇文本缓冲、Ink 的 string-width、WezTerm / kitty
//! 的口径一致(emoji 部分),所以是把分歧**收窄**而不是制造分歧。
//!
//! # 为什么不把整个 ambiguous 类都按宽算
//!
//! 「A」类里有制表符 `─│┌┐`(U+2500..)、块元素 `▁▂█`、希腊/西里尔字母、`±×÷`、
//! 箭头 `→←`、弯引号与省略号 —— Claude Code 的对话框边框全是制表符,进度条是块元素,
//! 而它们排版靠的 string-width 按 1 列算;整类改宽等于把每个 TUI 骨架撑爆。所以
//! [`FORCED_WIDE_RANGES`] 只收两批:枚举符号块(①⑴ⓐⅠ),以及中文排版里约定俗成的
//! 全角符号(※ ℃ № ‰ ★☆ ●○◎ ■□ ◆◇ ▲△▼▽ ◢◣◤◥,GB2312 一区那批,用户点名)。
//! 后一批里 `●○■◆` 也见于 clack / charm 这类交互式提示的骨架,那些场合会错一列——
//! 这是用户明确要的取舍。要扩就加一行。
//!
//! # 落格手法:借道占位字符
//!
//! `Term::input` 里宽字符那套簿记(行尾放不下时的 LEADING_WIDE_CHAR_SPACER + 折行、
//! INSERT 模式右移、WIDE_CHAR / WIDE_CHAR_SPACER 标志、覆盖旧宽字符时的清理)全是
//! 私有方法,复刻一遍等于 fork。这里先喂一个**确定为 2 列**的占位字符
//! ([`PLACEHOLDER`],全角空格)让 alacritty 把簿记做完,再回头把落下的那一格换成
//! 真身。落点由喂完后的光标位置反推:正常写完光标停在宽字符后两列;写到行尾时
//! alacritty 不推光标而是置 `input_needs_wrap`,光标停在 spacer 上、宽字符在前一列。
//!
//! 就地拓宽同理:基字右边那一格经 `Term::input(' ')` 写成 spacer(INSERT 模式右移、
//! 覆盖旧宽字符等簿记照常发生),再给基字打 WIDE_CHAR;基字已在最后一列时按 alacritty
//! 的规矩留 LEADING spacer、整簇搬到下一行行首。
//!
//! # 与上游应用的宽度分歧(只剩 A 类那一条)
//!
//! ConPTY / string-width 仍按 1 列算 ①,所以一行里 ① 之后的内容在我们这里比它们
//! 以为的靠右一列——整行重绘时没有可见后果,局部重绘(CUP 到行中再写)与光标定位
//! 会错一列。这是用户明确要的取舍:压字比错一列难看。
//!
//! # 已知不管的
//!
//! - Prepend 类字符(阿拉伯数字符号 U+0600 等)自身 0 宽却是簇的**开头**,alacritty
//!   会把它贴到前一格,随后的数字再被这里接进同一格——终端里几乎不出现,记档不修。
//! - 簇的字符串宽度 ≥ 3(个别印度系合体、高棉 BEYYAL)封顶按 2 列。

use alacritty_terminal::event::EventListener;
use alacritty_terminal::index::Column;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::cursor_icon::CursorIcon;
use alacritty_terminal::vte::ansi::{
    Attr, CharsetIndex, ClearMode, CursorShape, CursorStyle, Handler, Hyperlink, KeyboardModes,
    KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, PrivateMode, Rgb,
    ScpCharPath, ScpUpdateMode, StandardCharset, TabulationClearMode,
};
use unicode_segmentation::GraphemeCursor;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// 强制按 2 列落格的码位区间(闭区间)。
///
/// 只做**门禁**:最终还要过 `width_cjk() == 2` 这一关,区间里夹着的中性(N)
/// 码位与本来就是 W 的码位都不会被这里改动。
const FORCED_WIDE_RANGES: &[(u32, u32)] = &[
    // ── 枚举符号块
    // Number Forms:罗马数字 Ⅰ…Ⅻ / ⅰ…ⅹ
    (0x2160, 0x217F),
    // Enclosed Alphanumerics:①…⑳ ⑴…⒇ ⒈…⒛ ⓐ…ⓩ Ⓐ…Ⓩ ⓪ ⓫…⓴ ⓵…⓾ ⓿
    (0x2460, 0x24FF),
    // Enclosed Alphanumeric Supplement:🄀…🄊 🄐…🄩 🅐…🅩 🅰…🆉
    (0x1F100, 0x1F1FF),
    // ── 中文排版里的全角符号(GB2312 一区那批,用户点名):CJK 字体一律画成全角,
    //    塞进一列就压住后一个字。**只收这些**,同区里的 ± × ÷ ° → ← … “” 不动 ——
    //    那些在代码 / TUI / markdown 里到处都是,string-width 按 1 列排版。
    //    ¥(U+00A5)不在此列:它是 N 类窄字符,中文排版用的人民币符是全角 ￥(U+FFE5),
    //    本来就 2 列。
    (0x2030, 0x2030), // ‰
    (0x203B, 0x203B), // ※
    (0x2103, 0x2103), // ℃
    (0x2109, 0x2109), // ℉
    (0x2116, 0x2116), // №
    (0x2605, 0x2606), // ★ ☆
    (0x25A0, 0x25A1), // ■ □
    (0x25B2, 0x25B3), // ▲ △
    (0x25BC, 0x25BD), // ▼ ▽
    (0x25C6, 0x25C7), // ◆ ◇
    (0x25CB, 0x25CB), // ○
    (0x25CE, 0x25CF), // ◎ ●
    (0x25E2, 0x25E5), // ◢ ◣ ◤ ◥
];

/// 借道用的占位字符:U+3000 全角空格,unicode-width 口径下稳定为 2 列。
const PLACEHOLDER: char = '\u{3000}';

/// 一格里最多攒多少个字符(基字 + 0 宽字符)。
///
/// RGI emoji 序列最长在 10 个码位上下(带肤色的家庭 / 亲吻序列、tag 序列旗帜),
/// 这个上限是给恶意流兜底的:超过就当簇边界另起一格,不让一格无限长。
const MAX_CLUSTER_CHARS: usize = 16;

/// 这个字符是否要改按 2 列落格。
///
/// 三个条件缺一不可:在 [`FORCED_WIDE_RANGES`] 里、默认口径是 1 列(即属于
/// ambiguous 类,不是 0 宽也不是本来就宽)、CJK 口径是 2 列。
pub fn forced_wide(c: char) -> bool {
    let cp = c as u32;
    FORCED_WIDE_RANGES
        .iter()
        .any(|&(lo, hi)| (lo..=hi).contains(&cp))
        && c.width() == Some(1)
        && c.width_cjk() == Some(2)
}

/// 整簇的列宽(1 或 2)。基字命中 [`forced_wide`] 就是 2,否则取 unicode-width 的
/// 字符串口径(它认得 emoji 呈现 / 修饰 / ZWJ 序列),封顶 2。
fn cluster_width(cluster: &str) -> usize {
    if cluster.chars().next().is_some_and(forced_wide) {
        return 2;
    }
    cluster.width().min(2)
}

/// `c` 是否延续 `cluster`(光标前那一格里的字位簇)——UAX #29 扩展字位簇口径。
///
/// 两个 ASCII 之间永远是边界(Extend / ZWJ / Prepend / 区域指示符没有一个是 ASCII),
/// 这条快路径让纯英文输出不必进 `GraphemeCursor`。
fn extends_cluster(cluster: &str, c: char) -> bool {
    if c.is_ascii() && cluster.is_ascii() {
        return false;
    }
    // `cluster` 此时已经带上了 `c`(调用方拼好的),边界位置在 `c` 之前
    let split = cluster.len() - c.len_utf8();
    let mut cursor = GraphemeCursor::new(split, cluster.len(), true);
    !cursor.is_boundary(cluster, 0).unwrap_or(true)
}

/// 套在 `Term` 外面的 `Handler`:只拦 `input`,其余全部原样转发。
///
/// ⚠️ 升级 `vte` 时对照 `Handler` 的方法清单——新增的方法有默认空实现,
/// 漏转发不会编译报错,只会让那条序列静默失效。
pub(crate) struct WidthOverride<'a, T> {
    term: &'a mut Term<T>,
    /// 簇文本的暂存,跨字符复用免得每个字符分配一次。
    cluster: &'a mut String,
}

impl<'a, T: EventListener> WidthOverride<'a, T> {
    pub(crate) fn new(term: &'a mut Term<T>, cluster: &'a mut String) -> Self {
        Self { term, cluster }
    }

    /// 光标前那一格——可能是当前字位簇的落点。`None` = 行首,没有前一格。
    ///
    /// 与 alacritty 自己贴 0 宽字符的定位一致:`input_needs_wrap` 时光标还停在
    /// 最后一列那格上;落在宽字符的 spacer 上就再退一格。
    fn anchor(&self) -> Option<Column> {
        let grid = self.term.grid();
        let cursor = &grid.cursor;
        let mut col = cursor.point.column.0;
        if !cursor.input_needs_wrap {
            col = col.checked_sub(1)?;
        }
        if grid[cursor.point.line][Column(col)]
            .flags
            .contains(Flags::WIDE_CHAR_SPACER)
        {
            col = col.checked_sub(1)?;
        }
        Some(Column(col))
    }

    /// 把 `c` 接进 `col` 那一格的簇;簇宽从 1 变 2 就地拓宽。
    /// 前提:`self.cluster` 已是「旧簇 + c」。
    fn extend(&mut self, col: Column, c: char) {
        let want = cluster_width(self.cluster);
        let grid = self.term.grid_mut();
        let line = grid.cursor.point.line;
        let cell = &mut grid[line][col];
        cell.push_zerowidth(c);
        if want == 2 && !cell.flags.contains(Flags::WIDE_CHAR) {
            self.widen(col);
        }
    }

    /// 把 `col` 那一格从 1 列拓成 2 列。光标此刻要么在它右边一格,要么
    /// (`input_needs_wrap`)就停在它上面——那是它在最后一列的情形。
    fn widen(&mut self, col: Column) {
        let grid = self.term.grid_mut();
        let line = grid.cursor.point.line;
        if grid.cursor.input_needs_wrap {
            // 右边没有位置。折行关着就只能维持窄格(alacritty 对宽字符也是丢字);
            // 开着就照它的规矩:这一格留 LEADING spacer,整簇搬到下一行行首。
            if !self.term.mode().contains(TermMode::LINE_WRAP) {
                return;
            }
            let cell = &mut self.term.grid_mut()[line][col];
            let base = cell.c;
            let zerowidth: Vec<char> = cell.zerowidth().map(<[char]>::to_vec).unwrap_or_default();
            cell.clear_wide(); // c = ' ',清 zerowidth
            cell.flags.insert(Flags::LEADING_WIDE_CHAR_SPACER);
            self.write_wide(base, &zerowidth);
            return;
        }
        // 光标在基字右边那一格:经 `Term::input` 把它写成 spacer,让 INSERT 模式右移、
        // 覆盖旧宽字符的清理等簿记照常发生,再给基字打 WIDE_CHAR。
        grid.cursor.template.flags.insert(Flags::WIDE_CHAR_SPACER);
        self.term.input(' ');
        let grid = self.term.grid_mut();
        grid.cursor.template.flags.remove(Flags::WIDE_CHAR_SPACER);
        grid[line][col].flags.insert(Flags::WIDE_CHAR);
    }

    /// 以 2 列写下 `base`(带上它的 0 宽字符):借道占位字符,再把落下的那格换成真身。
    fn write_wide(&mut self, base: char, zerowidth: &[char]) {
        let before = self.term.grid().cursor.point;
        self.term.input(PLACEHOLDER);

        let grid = self.term.grid_mut();
        let cursor = &grid.cursor;
        // LINE_WRAP 关着且光标已在最后一列:`Term::input` 一个格子都不写、只置
        // `input_needs_wrap` 就返回(见其源码),此时光标纹丝不动——我们也不动。
        if cursor.point == before {
            return;
        }
        let back = if cursor.input_needs_wrap { 1 } else { 2 };
        let Some(col) = cursor.point.column.0.checked_sub(back) else {
            return;
        };
        let line = cursor.point.line;
        let cell = &mut grid[line][Column(col)];
        // 双重校验:落点必须正是刚写下的占位宽字符,否则宁可留着全角空格也不乱改。
        if cell.c == PLACEHOLDER && cell.flags.contains(Flags::WIDE_CHAR) {
            cell.c = base;
            for &z in zerowidth {
                cell.push_zerowidth(z);
            }
        }
    }
}

/// 一格里的簇文本:基字 + 0 宽字符。
fn cell_cluster(cell: &Cell, out: &mut String) {
    out.clear();
    out.push(cell.c);
    if let Some(zw) = cell.zerowidth() {
        out.extend(zw);
    }
}

macro_rules! forward_to_term {
    ($( $name:ident ( $( $arg:ident : $ty:ty ),* ) ;)*) => {
        $(
            #[inline]
            fn $name(&mut self, $( $arg: $ty ),*) {
                self.term.$name($( $arg ),*)
            }
        )*
    };
}

impl<T: EventListener> Handler for WidthOverride<'_, T> {
    fn input(&mut self, c: char) {
        // ── 先看能不能接到光标前那一格的簇上
        if let Some(col) = self.anchor() {
            let line = self.term.grid().cursor.point.line;
            cell_cluster(&self.term.grid()[line][col], self.cluster);
            if self.cluster.chars().count() >= MAX_CLUSTER_CHARS {
                // 那一格已经满了:0 宽字符直接丢(交给 alacritty 它照样会贴上去),
                // 其余字符另起一格
                if c.width() == Some(0) {
                    return;
                }
            } else {
                self.cluster.push(c);
                if extends_cluster(self.cluster, c) {
                    self.extend(col, c);
                    return;
                }
            }
        }
        // ── 另起一簇
        if c.width() == Some(1) && forced_wide(c) {
            self.write_wide(c, &[]);
        } else {
            // 0 宽的孤儿组合符号也走这里:alacritty 原生就是贴前一格
            self.term.input(c);
        }
    }

    forward_to_term! {
        set_title(title: Option<String>);
        set_cursor_style(style: Option<CursorStyle>);
        set_cursor_shape(shape: CursorShape);
        goto(line: i32, col: usize);
        goto_line(line: i32);
        goto_col(col: usize);
        insert_blank(count: usize);
        move_up(rows: usize);
        move_down(rows: usize);
        identify_terminal(intermediate: Option<char>);
        device_status(arg: usize);
        move_forward(cols: usize);
        move_backward(cols: usize);
        move_down_and_cr(rows: usize);
        move_up_and_cr(rows: usize);
        put_tab(count: u16);
        backspace();
        carriage_return();
        linefeed();
        bell();
        substitute();
        newline();
        set_horizontal_tabstop();
        scroll_up(rows: usize);
        scroll_down(rows: usize);
        insert_blank_lines(count: usize);
        delete_lines(count: usize);
        erase_chars(count: usize);
        delete_chars(count: usize);
        move_backward_tabs(count: u16);
        move_forward_tabs(count: u16);
        save_cursor_position();
        restore_cursor_position();
        clear_line(mode: LineClearMode);
        clear_screen(mode: ClearMode);
        clear_tabs(mode: TabulationClearMode);
        set_tabs(interval: u16);
        reset_state();
        reverse_index();
        terminal_attribute(attr: Attr);
        set_mode(mode: Mode);
        unset_mode(mode: Mode);
        report_mode(mode: Mode);
        set_private_mode(mode: PrivateMode);
        unset_private_mode(mode: PrivateMode);
        report_private_mode(mode: PrivateMode);
        set_scrolling_region(top: usize, bottom: Option<usize>);
        set_keypad_application_mode();
        unset_keypad_application_mode();
        set_active_charset(index: CharsetIndex);
        configure_charset(index: CharsetIndex, charset: StandardCharset);
        set_color(index: usize, color: Rgb);
        dynamic_color_sequence(prefix: String, index: usize, terminator: &str);
        reset_color(index: usize);
        clipboard_store(clipboard: u8, base64: &[u8]);
        clipboard_load(clipboard: u8, terminator: &str);
        decaln();
        push_title();
        pop_title();
        text_area_size_pixels();
        text_area_size_chars();
        set_hyperlink(hyperlink: Option<Hyperlink>);
        set_mouse_cursor_icon(icon: CursorIcon);
        report_keyboard_mode();
        push_keyboard_mode(mode: KeyboardModes);
        pop_keyboard_modes(to_pop: u16);
        set_keyboard_mode(mode: KeyboardModes, behavior: KeyboardModesApplyBehavior);
        set_modify_other_keys(mode: ModifyOtherKeys);
        report_modify_other_keys();
        set_scp(char_path: ScpCharPath, update_mode: ScpUpdateMode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TermSize, TerminalEmulator};
    use alacritty_terminal::index::{Line, Point};

    /// 某一行的 `(列号, 字符)`,行尾空格裁掉——`visible_columns` 本身不裁。
    fn cols(e: &TerminalEmulator, row: usize) -> Vec<(usize, char)> {
        let mut v = e.visible_columns().swap_remove(row);
        while v.last().is_some_and(|&(_, c)| c == ' ') {
            v.pop();
        }
        v
    }

    /// 某一格的完整簇文本(基字 + 0 宽字符)与是否宽格。
    fn cell_at(e: &TerminalEmulator, row: i32, col: usize) -> (String, bool) {
        e.with_term(|t| {
            let cell = &t.grid()[Line(row)][Column(col)];
            let mut s = String::new();
            cell_cluster(cell, &mut s);
            (s, cell.flags.contains(Flags::WIDE_CHAR))
        })
    }

    // ───────────────────────── A 类符号 ─────────────────────────

    /// 占位字符本身必须是 unicode-width 口径下的 2 列,否则整个借道手法不成立。
    #[test]
    fn 占位字符稳定为两列() {
        assert_eq!(PLACEHOLDER.width(), Some(2));
        assert_eq!(PLACEHOLDER.width_cjk(), Some(2));
    }

    /// 表里的每个区间都必须真的含有 ambiguous 码位——防止有人加了一段全是 N 或 W
    /// 的区间,以为生效了其实一个字符都没改。
    #[test]
    fn 每个区间都至少命中一个字符() {
        for &(lo, hi) in FORCED_WIDE_RANGES {
            let hit = (lo..=hi)
                .filter_map(char::from_u32)
                .any(forced_wide);
            assert!(hit, "U+{lo:04X}..=U+{hi:04X} 一个字符都没命中");
        }
    }

    /// 用户点名的那一类:带圈数字、括号数字、带圈字母、罗马数字。
    #[test]
    fn 枚举符号按宽算() {
        for c in ['①', '⑳', '⑴', '⒈', 'ⓐ', 'Ⓩ', '⓪', '⓿', 'Ⅰ', 'Ⅻ', 'ⅹ', '🄐', '🅐'] {
            assert!(forced_wide(c), "{c} (U+{:04X}) 应按 2 列", c as u32);
        }
        // 区间只是门禁:Number Forms 里 Ⅼ Ⅽ Ⅾ Ⅿ / ⅻ 这些是 N 类,不动
        for c in ['Ⅼ', 'ⅻ'] {
            assert!(!forced_wide(c), "{c} (U+{:04X}) 是 N 类,不该改宽", c as u32);
        }
    }

    /// 用户点名的第二批:中文排版的全角符号(GB2312 一区),CJK 字体一律画成全角。
    #[test]
    fn 中文全角符号按宽算() {
        for c in [
            '※', '℃', '℉', '№', '‰', '★', '☆', '●', '○', '◎', '■', '□', '◆', '◇', '▲', '△',
            '▼', '▽', '◢', '◣', '◤', '◥',
        ] {
            assert!(forced_wide(c), "{c} (U+{:04X}) 应按 2 列", c as u32);
        }
        // ¥(U+00A5)是 N 类窄字符,中文用的是全角 ￥(U+FFE5),后者本来就 2 列
        assert!(!forced_wide('¥'));
        assert_eq!('￥'.width(), Some(2));
    }

    /// 反例:同属 ambiguous 类却**不能**改宽的——TUI 骨架用的制表符与块元素、
    /// 箭头、播放 / 光标标记 `▶◀`、`±×÷°`、弯引号省略号、希腊西里尔字母;
    /// 以及本来就是 W 的 CJK / 0 宽的组合符号。
    #[test]
    fn 其它字符原样放行() {
        for c in [
            '─', '│', '╭', '█', '▁', '→', '←', '▶', '◀', 'α', 'Я', '±', '×', '÷', '°', '·',
            '“', '”', '…', '—', '•', 'a', ' ',
        ] {
            assert!(!forced_wide(c), "{c} (U+{:04X}) 不该被改宽", c as u32);
        }
        assert!(!forced_wide('中'), "本来就是 W,不经这里");
        assert!(!forced_wide('\u{301}'), "组合符号是 0 宽");
        assert!(!forced_wide('㉑'), "U+3251 本来就是 W");
    }

    /// 核心验收:`①abc` 里 `a` 必须落在第 2 列,与 `中abc` 同款。
    #[test]
    fn 带圈数字占两列() {
        let e = TerminalEmulator::new(TermSize::new(20, 3));
        e.advance("①abc".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, '①'), (2, 'a'), (3, 'b'), (4, 'c')]);
        e.with_term(|t| {
            let row = &t.grid()[Line(0)];
            assert!(row[Column(0)].flags.contains(Flags::WIDE_CHAR));
            assert!(row[Column(1)].flags.contains(Flags::WIDE_CHAR_SPACER));
            assert_eq!(t.grid().cursor.point, Point::new(Line(0), Column(5)));
        });
        assert_eq!(e.visible_lines()[0], "①abc", "读回口不出现占位字符");
    }

    /// 对照组:制表符、块元素、箭头仍是 1 列——Claude Code 的边框与进度条不能被撑开。
    #[test]
    fn 制表符与箭头仍是一列() {
        let e = TerminalEmulator::new(TermSize::new(20, 3));
        e.advance("─█→x".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, '─'), (1, '█'), (2, '→'), (3, 'x')]);
    }

    /// 全角符号在 grid 里真的占两列,与 ① 同一条路。
    #[test]
    fn 全角符号占两列() {
        let e = TerminalEmulator::new(TermSize::new(20, 3));
        e.advance("※a●b℃c".as_bytes());
        assert_eq!(
            cols(&e, 0),
            vec![(0, '※'), (2, 'a'), (3, '●'), (5, 'b'), (6, '℃'), (8, 'c')]
        );
    }

    /// 行尾只剩一列时走 alacritty 自己的折行簿记:前一行末尾留 LEADING 占位,
    /// 字符落到下一行开头。
    #[test]
    fn 行尾放不下时折到下一行() {
        let e = TerminalEmulator::new(TermSize::new(4, 3));
        e.advance("abc①d".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, 'a'), (1, 'b'), (2, 'c')]);
        assert_eq!(cols(&e, 1), vec![(0, '①'), (2, 'd')]);
        e.with_term(|t| {
            let row0 = &t.grid()[Line(0)];
            assert!(row0[Column(3)].flags.contains(Flags::LEADING_WIDE_CHAR_SPACER));
        });
    }

    /// 恰好填满最后两列:光标不推进而是置 `input_needs_wrap`,落点反推走的是
    /// 「光标停在 spacer 上」那条分支。
    #[test]
    fn 恰好填满行尾两列() {
        let e = TerminalEmulator::new(TermSize::new(4, 3));
        e.advance("ab①".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, 'a'), (1, 'b'), (2, '①')]);
        e.with_term(|t| {
            assert!(t.grid().cursor.input_needs_wrap);
            assert_eq!(t.grid().cursor.point.column, Column(3));
        });
        e.advance(b"z");
        assert_eq!(cols(&e, 1), vec![(0, 'z')], "后续字符正常折到下一行");
    }

    /// LINE_WRAP 关着(DECAWM reset)、光标已在最后一列:alacritty 丢字不写格,
    /// 我们不能把旁边的格子误改成 ①。
    #[test]
    fn 关折行时行尾丢字不污染邻格() {
        let e = TerminalEmulator::new(TermSize::new(4, 3));
        e.advance("\x1b[?7labc①".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, 'a'), (1, 'b'), (2, 'c')]);
        assert_eq!(e.visible_lines()[0], "abc", "没有占位字符残留");
    }

    /// INSERT 模式(IRM)下宽字符把右侧内容整体推两列。
    #[test]
    fn 插入模式推两列() {
        let e = TerminalEmulator::new(TermSize::new(10, 3));
        e.advance("abcd\r\x1b[4h①".as_bytes());
        assert_eq!(
            cols(&e, 0),
            vec![(0, '①'), (2, 'a'), (3, 'b'), (4, 'c'), (5, 'd')]
        );
    }

    /// 覆盖写:在 ① 的 spacer 列写窄字符,alacritty 会把宽字符一并清掉——
    /// 这是它对 CJK 的既有语义,借道之后对 ① 同样成立,不能留半个。
    #[test]
    fn 覆盖_spacer_列时宽字符一并清掉() {
        let e = TerminalEmulator::new(TermSize::new(10, 3));
        e.advance("①bc\x1b[2Gx".as_bytes());
        e.with_term(|t| {
            let row = &t.grid()[Line(0)];
            assert!(!row[Column(0)].flags.contains(Flags::WIDE_CHAR));
            assert_eq!(row[Column(0)].c, ' ');
            assert_eq!(row[Column(1)].c, 'x');
        });
    }

    /// 转发面:随手抽几条经过包装层的非 `input` 序列,确认簿记没被拦掉。
    #[test]
    fn 其它序列原样转发() {
        let e = TerminalEmulator::new(TermSize::new(10, 3));
        e.advance(b"abc\x1b[2D\x1b[1P");
        assert_eq!(e.visible_lines()[0], "ac", "CUB + DCH");
        e.advance(b"\x1b]0;hello\x07");
        assert!(
            e.events().drain().iter().any(|ev| matches!(
                ev,
                alacritty_terminal::event::Event::Title(t) if t == "hello"
            )),
            "OSC 0 标题事件"
        );
    }

    // ───────────────────────── emoji 序列 ─────────────────────────

    /// 呈现序列:❤ 是 1 列的文本符号,跟上 VS16 就是 2 列的 emoji——那一格要就地拓宽。
    #[test]
    fn 呈现序列拓成两列() {
        let e = TerminalEmulator::new(TermSize::new(20, 3));
        e.advance("❤\u{FE0F}x".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, '❤'), (2, 'x')]);
        assert_eq!(cell_at(&e, 0, 0), ("❤\u{FE0F}".into(), true));
        e.with_term(|t| {
            assert!(t.grid()[Line(0)][Column(1)].flags.contains(Flags::WIDE_CHAR_SPACER));
        });
    }

    /// 肤色修饰符自身是 W 类(2 列),alacritty 会另起一个宽格画成色块;
    /// 它得进 👍 那一格,整簇仍是 2 列。
    #[test]
    fn 肤色修饰符并入基字() {
        let e = TerminalEmulator::new(TermSize::new(20, 3));
        e.advance("👍🏽x".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, '👍'), (2, 'x')]);
        assert_eq!(cell_at(&e, 0, 0), ("👍🏽".into(), true));
    }

    /// ZWJ 序列:三口之家是一格 2 列,不是三格 6 列。
    #[test]
    fn zwj_序列整簇一格() {
        let e = TerminalEmulator::new(TermSize::new(20, 3));
        e.advance("👨\u{200D}👩\u{200D}👧x".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, '👨'), (2, 'x')]);
        assert_eq!(cell_at(&e, 0, 0), ("👨\u{200D}👩\u{200D}👧".into(), true));
    }

    /// 彩虹旗:🏳(1 列的文本符号)+ VS16 + ZWJ + 🌈。前两步把格子拓宽,
    /// 后两步接进同一格。
    #[test]
    fn 彩虹旗整簇一格() {
        let e = TerminalEmulator::new(TermSize::new(20, 3));
        e.advance("🏳\u{FE0F}\u{200D}🌈x".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, '🏳'), (2, 'x')]);
        assert_eq!(cell_at(&e, 0, 0), ("🏳\u{FE0F}\u{200D}🌈".into(), true));
    }

    /// 区域指示符成对成旗:两个各 1 列的字符并成一格 2 列;第三个另起。
    #[test]
    fn 区域指示符成对() {
        let e = TerminalEmulator::new(TermSize::new(20, 3));
        e.advance("🇨🇳🇺🇸x".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, '🇨'), (2, '🇺'), (4, 'x')]);
        assert_eq!(cell_at(&e, 0, 0), ("🇨🇳".into(), true));
        assert_eq!(cell_at(&e, 0, 2), ("🇺🇸".into(), true));
    }

    /// 键帽序列 `1️⃣`:ASCII 基字 + VS16 + 组合键帽,拓成 2 列。
    #[test]
    fn 键帽序列拓成两列() {
        let e = TerminalEmulator::new(TermSize::new(20, 3));
        e.advance("1\u{FE0F}\u{20E3}x".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, '1'), (2, 'x')]);
        assert_eq!(cell_at(&e, 0, 0), ("1\u{FE0F}\u{20E3}".into(), true));
    }

    /// 本来就是 W 的 emoji 不受影响,连着来也各占各的格。
    #[test]
    fn 宽_emoji_照旧() {
        let e = TerminalEmulator::new(TermSize::new(20, 3));
        e.advance("🎉✅x".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, '🎉'), (2, '✅'), (4, 'x')]);
    }

    /// 普通组合符号(é = e + 锐音符)仍是 1 列;文本呈现序列(⚡ + VS15)不会**缩**格。
    #[test]
    fn 组合符号不拓宽_文本呈现不缩格() {
        let e = TerminalEmulator::new(TermSize::new(20, 3));
        e.advance("e\u{301}x⚡\u{FE0E}y".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, 'e'), (1, 'x'), (2, '⚡'), (4, 'y')]);
        assert_eq!(cell_at(&e, 0, 0), ("e\u{301}".into(), false));
        assert_eq!(cell_at(&e, 0, 2), ("⚡\u{FE0E}".into(), true));
    }

    /// 拓宽时基字恰好在最后一列:这一格变 LEADING spacer,整簇(含 VS16)搬到下一行行首。
    #[test]
    fn 行尾拓宽整簇搬到下一行() {
        let e = TerminalEmulator::new(TermSize::new(4, 3));
        e.advance("abc❤\u{FE0F}x".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, 'a'), (1, 'b'), (2, 'c')]);
        assert_eq!(cols(&e, 1), vec![(0, '❤'), (2, 'x')]);
        assert_eq!(cell_at(&e, 1, 0), ("❤\u{FE0F}".into(), true));
        e.with_term(|t| {
            let tail = &t.grid()[Line(0)][Column(3)];
            assert!(tail.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER));
            assert!(tail.zerowidth().is_none_or(<[char]>::is_empty), "旧格的 0 宽字符要清干净");
        });
    }

    /// 拓宽时基字在倒数第二列:spacer 落在最后一列,光标置 `input_needs_wrap`,
    /// 后续字符折到下一行——与直接写一个宽字符到那个位置的结果完全一样。
    #[test]
    fn 倒数第二列拓宽后正常折行() {
        let e = TerminalEmulator::new(TermSize::new(4, 3));
        e.advance("ab❤\u{FE0F}x".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, 'a'), (1, 'b'), (2, '❤')]);
        assert_eq!(cols(&e, 1), vec![(0, 'x')]);
    }

    /// 折行关着、基字在最后一列:拓不了宽,维持窄格(alacritty 对宽字符是直接丢字,
    /// 我们至少把字留住),后续字符按它的规矩丢掉。
    #[test]
    fn 关折行时行尾不拓宽() {
        let e = TerminalEmulator::new(TermSize::new(4, 3));
        e.advance("\x1b[?7labc❤\u{FE0F}".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, 'a'), (1, 'b'), (2, 'c'), (3, '❤')]);
        assert_eq!(cell_at(&e, 0, 3), ("❤\u{FE0F}".into(), false));
    }

    /// INSERT 模式下拓宽:spacer 那一步也经 `Term::input`,右侧内容再推一列——
    /// 总共推两列,与直接写宽字符一致。
    #[test]
    fn 插入模式下拓宽推两列() {
        let e = TerminalEmulator::new(TermSize::new(10, 3));
        e.advance("abcd\r\x1b[4h❤\u{FE0F}".as_bytes());
        assert_eq!(
            cols(&e, 0),
            vec![(0, '❤'), (2, 'a'), (3, 'b'), (4, 'c'), (5, 'd')]
        );
    }

    /// 拓宽覆盖到右边一个旧宽字符的首格:那个宽字符的 spacer 要被 alacritty 的
    /// 覆盖逻辑清掉,不能留一个孤儿 spacer。
    #[test]
    fn 拓宽覆盖旧宽字符时清其_spacer() {
        let e = TerminalEmulator::new(TermSize::new(10, 3));
        // 先写 "x中",再回到第 1 列写 ❤ + VS16:❤ 覆盖 x 的位置不动,拓宽把 spacer
        // 写到「中」的首格 → 「中」整个被清
        e.advance("x中\x1b[2G❤\u{FE0F}".as_bytes());
        assert_eq!(cols(&e, 0), vec![(0, 'x'), (1, '❤')]);
        e.with_term(|t| {
            let row = &t.grid()[Line(0)];
            assert!(row[Column(2)].flags.contains(Flags::WIDE_CHAR_SPACER));
            assert!(!row[Column(3)].flags.contains(Flags::WIDE_CHAR_SPACER), "「中」的旧 spacer 已清");
        });
    }

    /// 簇长度封顶:超过上限的 0 宽字符另起一格,一格不会被恶意流撑到无限长。
    #[test]
    fn 簇长度封顶() {
        let e = TerminalEmulator::new(TermSize::new(40, 3));
        let mut s = String::from("a");
        for _ in 0..(MAX_CLUSTER_CHARS + 3) {
            s.push('\u{301}');
        }
        s.push('b');
        e.advance(s.as_bytes());
        let (cluster, _) = cell_at(&e, 0, 0);
        assert_eq!(cluster.chars().count(), MAX_CLUSTER_CHARS);
        assert_eq!(cols(&e, 0).last(), Some(&(1, 'b')), "b 仍在第 1 列");
    }

    /// 簇的判定跨 `advance` 批次也成立——PTY 读缓冲在 ZWJ 序列中间切一刀是常事。
    #[test]
    fn 跨批次仍能接簇() {
        let e = TerminalEmulator::new(TermSize::new(20, 3));
        let bytes = "👨\u{200D}👩\u{200D}👧x".as_bytes();
        for chunk in bytes.chunks(3) {
            e.advance(chunk);
        }
        assert_eq!(cols(&e, 0), vec![(0, '👨'), (2, 'x')]);
        assert_eq!(cell_at(&e, 0, 0), ("👨\u{200D}👩\u{200D}👧".into(), true));
    }

    /// 中文与 emoji 混排的整体对齐:每个 CJK 与每个簇都是 2 列。
    #[test]
    fn 中文与_emoji_混排对齐() {
        let e = TerminalEmulator::new(TermSize::new(40, 3));
        e.advance("中❤\u{FE0F}文👍🏽字🇨🇳①x".as_bytes());
        assert_eq!(
            cols(&e, 0),
            vec![(0, '中'), (2, '❤'), (4, '文'), (6, '👍'), (8, '字'), (10, '🇨'), (12, '①'), (14, 'x')]
        );
    }
}
