//! Blit-host mode — same UI as the crossterm path, but rendered into a
//! ratatui `TestBackend` and shipped as diff'd cell frames over a Unix
//! socket. The renderer (tmnl, or mnml's `pane_host`) sends back resize
//! + key/mouse events on the same socket.
//!
//! Modeled after `mixr-private/src/tui/blit.rs` — kept deliberately
//! close so the two stay easy to keep in sync.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Mutex;
use std::sync::mpsc::{TryRecvError, channel};
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow};
use crossterm::event::{KeyCode as CtKeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use tmnl_protocol::{
    DiffRun, Frame, InputEvent, KeyCode as WireKeyCode, KeyInput, MOD_ALT, MOD_CTRL, MOD_SHIFT,
    MOD_SUPER, Message, PROTOCOL_VERSION, WireCell, pack_rgba_u8, read_message, write_message,
};
use tokio::time::sleep;

use crate::app::App;
use crate::keys;
use crate::ui::draw;

const POLL_SLEEP_MS: u64 = 16;
const INITIAL_RESIZE_TIMEOUT: Duration = Duration::from_secs(5);

const ATTR_BOLD: u32 = 1 << 0;
const ATTR_DIM: u32 = 1 << 1;
const ATTR_ITALIC: u32 = 1 << 2;
const ATTR_UNDERLINE: u32 = 1 << 3;
const ATTR_REVERSED: u32 = 1 << 4;
const ATTR_CROSSED_OUT: u32 = 1 << 5;

pub async fn run(app: &mut App, socket: &Path) -> Result<()> {
    let conn = UnixStream::connect(socket)
        .map_err(|e| anyhow!("blit: connect {}: {e}", socket.display()))?;
    let reader_stream = conn
        .try_clone()
        .map_err(|e| anyhow!("blit: clone stream: {e}"))?;
    let writer = Mutex::new(conn);

    // Handshake.
    {
        let mut w = writer.lock().unwrap();
        write_message(
            &mut *w,
            &Message::Hello {
                version: PROTOCOL_VERSION,
            },
        )
        .map_err(|e| anyhow!("blit: hello: {e}"))?;
    }

    let (resize_tx, resize_rx) = channel::<(u16, u16)>();
    let (input_tx, input_rx) = channel::<InputEvent>();
    let (quit_tx, quit_rx) = channel::<()>();
    let (disc_tx, disc_rx) = channel::<()>();
    thread::spawn(move || {
        let mut r = BufReader::new(reader_stream);
        loop {
            match read_message(&mut r) {
                Ok(Message::Resize(rz)) => {
                    if resize_tx.send((rz.cols, rz.rows)).is_err() {
                        break;
                    }
                }
                Ok(Message::Input(ev)) => {
                    if input_tx.send(ev).is_err() {
                        break;
                    }
                }
                Ok(Message::Quit) => {
                    let _ = quit_tx.send(());
                    break;
                }
                Ok(_) => {}
                Err(_) => {
                    let _ = disc_tx.send(());
                    break;
                }
            }
        }
    });

    // Wait for the first Resize so we build the TestBackend at the
    // right dims.
    let (mut cols, mut rows) = {
        let deadline = std::time::Instant::now() + INITIAL_RESIZE_TIMEOUT;
        loop {
            match resize_rx.try_recv() {
                Ok(p) => break p,
                Err(TryRecvError::Empty) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(anyhow!("blit: no Resize from server within 5s"));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(TryRecvError::Disconnected) => {
                    return Err(anyhow!("blit: server disconnected"));
                }
            }
        }
    };
    if cols == 0 || rows == 0 {
        return Err(anyhow!("blit: server reported empty grid {cols}x{rows}"));
    }

    let backend = TestBackend::new(cols, rows);
    let mut terminal = Terminal::new(backend).map_err(|e| anyhow!("blit: terminal: {e}"))?;

    let mut frame_seq: u64 = 0;
    let mut prev_cells: Vec<WireCell> = Vec::new();
    let mut prev_dims: (u16, u16) = (0, 0);
    let mut last_refresh = std::time::Instant::now();

    loop {
        // Most-recent-wins resize drain.
        let mut new_size: Option<(u16, u16)> = None;
        loop {
            match resize_rx.try_recv() {
                Ok(p) => new_size = Some(p),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }
        if let Some((nc, nr)) = new_size
            && (nc != cols || nr != rows)
            && nc > 0
            && nr > 0
        {
            cols = nc;
            rows = nr;
            terminal.backend_mut().resize(cols, rows);
            terminal
                .resize(Rect::new(0, 0, cols, rows))
                .map_err(|e| anyhow!("blit: resize: {e}"))?;
            prev_cells.clear();
        }

        if quit_rx.try_recv().is_ok() || disc_rx.try_recv().is_ok() {
            return Ok(());
        }

        // Drain input events. Translate to crossterm and feed
        // `keys::handle` + `keys::apply` — exactly what the stdout
        // loop does. We only care about keys here; mouse handling
        // can be added later if the app grows clickable surfaces.
        loop {
            match input_rx.try_recv() {
                Ok(InputEvent::Key(k)) => {
                    let ke = key_to_crossterm(&k);
                    if ke.code == CtKeyCode::Char('c')
                        && ke.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        return Ok(());
                    }
                    if let Some(action) = keys::handle(ke, app)
                        && keys::apply(action, app).await
                    {
                        return Ok(());
                    }
                }
                Ok(InputEvent::Mouse(_)) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }

        // Auto-refresh on interval, matching `ui::event_loop`.
        if app.cfg.refresh_interval_secs > 0
            && last_refresh.elapsed().as_secs() >= app.cfg.refresh_interval_secs
        {
            app.refresh_active();
            last_refresh = std::time::Instant::now();
        }
        app.drain();

        terminal
            .draw(|frame| draw(frame, app))
            .map_err(|e| anyhow!("blit: draw: {e}"))?;
        let cursor = terminal.get_cursor_position().ok();

        let buf = terminal.backend().buffer();
        let bw = buf.area.width;
        let bh = buf.area.height;
        let mut cells = Vec::with_capacity(bw as usize * bh as usize);
        for y in 0..bh {
            for x in 0..bw {
                let c = &buf[(x, y)];
                cells.push(WireCell {
                    ch: c.symbol().chars().next().unwrap_or(' ') as u32,
                    fg: color_to_rgba(c.fg, false),
                    bg: color_to_rgba(c.bg, true),
                    attrs: modifier_to_bits(c.modifier),
                });
            }
        }

        let runs = if prev_cells.len() != cells.len() || prev_dims != (bw, bh) {
            vec![DiffRun {
                start: 0,
                cells: cells.clone(),
            }]
        } else {
            compute_runs(&prev_cells, &cells)
        };
        prev_cells.clear();
        prev_cells.extend_from_slice(&cells);
        prev_dims = (bw, bh);

        let frame = Frame {
            seq: frame_seq,
            cols: bw,
            rows: bh,
            cursor_col: cursor.as_ref().map(|p| p.x).unwrap_or(0),
            cursor_row: cursor.as_ref().map(|p| p.y).unwrap_or(0),
            cursor_shape: 0,
            cursor_visible: u8::from(cursor.is_some()),
            runs,
        };
        frame_seq = frame_seq.wrapping_add(1);

        {
            let mut w = writer.lock().unwrap();
            if write_message(&mut *w, &Message::Frame(frame)).is_err() {
                return Ok(());
            }
        }

        sleep(Duration::from_millis(POLL_SLEEP_MS)).await;
    }
}

fn modifier_to_bits(m: Modifier) -> u32 {
    let mut a = 0u32;
    if m.contains(Modifier::BOLD) {
        a |= ATTR_BOLD;
    }
    if m.contains(Modifier::DIM) {
        a |= ATTR_DIM;
    }
    if m.contains(Modifier::ITALIC) {
        a |= ATTR_ITALIC;
    }
    if m.contains(Modifier::UNDERLINED) {
        a |= ATTR_UNDERLINE;
    }
    if m.contains(Modifier::REVERSED) {
        a |= ATTR_REVERSED;
    }
    if m.contains(Modifier::CROSSED_OUT) {
        a |= ATTR_CROSSED_OUT;
    }
    a
}

fn color_to_rgba(c: Color, is_bg: bool) -> u32 {
    match c {
        Color::Rgb(r, g, b) => pack_rgba_u8(r, g, b, 0xff),
        Color::Reset => {
            if is_bg {
                pack_rgba_u8(0x10, 0x11, 0x1c, 0xff)
            } else {
                pack_rgba_u8(0xab, 0xb2, 0xbf, 0xff)
            }
        }
        Color::Black => pack_rgba_u8(0x10, 0x11, 0x1c, 0xff),
        Color::Red => pack_rgba_u8(0xe0, 0x60, 0x60, 0xff),
        Color::Green => pack_rgba_u8(0x84, 0xc8, 0x6f, 0xff),
        Color::Yellow => pack_rgba_u8(0xee, 0xbb, 0x57, 0xff),
        Color::Blue => pack_rgba_u8(0x6e, 0xa2, 0xe7, 0xff),
        Color::Magenta => pack_rgba_u8(0xc9, 0x7a, 0xea, 0xff),
        Color::Cyan => pack_rgba_u8(0x5f, 0xb3, 0xa1, 0xff),
        Color::Gray => pack_rgba_u8(0xab, 0xb2, 0xbf, 0xff),
        Color::DarkGray => pack_rgba_u8(0x42, 0x46, 0x4e, 0xff),
        Color::LightRed => pack_rgba_u8(0xff, 0x82, 0x82, 0xff),
        Color::LightGreen => pack_rgba_u8(0xa6, 0xe2, 0x8c, 0xff),
        Color::LightYellow => pack_rgba_u8(0xff, 0xd7, 0x71, 0xff),
        Color::LightBlue => pack_rgba_u8(0x82, 0xb3, 0xff, 0xff),
        Color::LightMagenta => pack_rgba_u8(0xdc, 0xa5, 0xff, 0xff),
        Color::LightCyan => pack_rgba_u8(0x84, 0xd6, 0xc5, 0xff),
        Color::White => pack_rgba_u8(0xff, 0xff, 0xff, 0xff),
        Color::Indexed(i) => ansi256_to_rgba(i),
    }
}

fn ansi256_to_rgba(i: u8) -> u32 {
    if i < 16 {
        let palette = [
            (0x10, 0x11, 0x1c),
            (0xe0, 0x60, 0x60),
            (0x84, 0xc8, 0x6f),
            (0xee, 0xbb, 0x57),
            (0x6e, 0xa2, 0xe7),
            (0xc9, 0x7a, 0xea),
            (0x5f, 0xb3, 0xa1),
            (0xab, 0xb2, 0xbf),
            (0x42, 0x46, 0x4e),
            (0xff, 0x82, 0x82),
            (0xa6, 0xe2, 0x8c),
            (0xff, 0xd7, 0x71),
            (0x82, 0xb3, 0xff),
            (0xdc, 0xa5, 0xff),
            (0x84, 0xd6, 0xc5),
            (0xff, 0xff, 0xff),
        ];
        let (r, g, b) = palette[i as usize];
        pack_rgba_u8(r, g, b, 0xff)
    } else if i < 232 {
        let n = i - 16;
        let r = (n / 36) * 51;
        let g = ((n / 6) % 6) * 51;
        let b = (n % 6) * 51;
        pack_rgba_u8(r, g, b, 0xff)
    } else {
        let v = 8 + (i - 232) * 10;
        pack_rgba_u8(v, v, v, 0xff)
    }
}

fn unpack_mods(m: u8) -> KeyModifiers {
    let mut out = KeyModifiers::empty();
    if m & MOD_SHIFT != 0 {
        out |= KeyModifiers::SHIFT;
    }
    if m & MOD_CTRL != 0 {
        out |= KeyModifiers::CONTROL;
    }
    if m & MOD_ALT != 0 {
        out |= KeyModifiers::ALT;
    }
    if m & MOD_SUPER != 0 {
        out |= KeyModifiers::SUPER;
    }
    out
}

fn key_to_crossterm(k: &KeyInput) -> KeyEvent {
    let code = match k.code {
        WireKeyCode::Char(c) => CtKeyCode::Char(c),
        WireKeyCode::Backspace => CtKeyCode::Backspace,
        WireKeyCode::Enter => CtKeyCode::Enter,
        WireKeyCode::Left => CtKeyCode::Left,
        WireKeyCode::Right => CtKeyCode::Right,
        WireKeyCode::Up => CtKeyCode::Up,
        WireKeyCode::Down => CtKeyCode::Down,
        WireKeyCode::Home => CtKeyCode::Home,
        WireKeyCode::End => CtKeyCode::End,
        WireKeyCode::PageUp => CtKeyCode::PageUp,
        WireKeyCode::PageDown => CtKeyCode::PageDown,
        WireKeyCode::Tab => CtKeyCode::Tab,
        WireKeyCode::BackTab => CtKeyCode::BackTab,
        WireKeyCode::Delete => CtKeyCode::Delete,
        WireKeyCode::Insert => CtKeyCode::Insert,
        WireKeyCode::Esc => CtKeyCode::Esc,
        WireKeyCode::F(n) => CtKeyCode::F(n),
    };
    KeyEvent {
        code,
        modifiers: unpack_mods(k.mods),
        kind: KeyEventKind::Press,
        state: KeyEventState::empty(),
    }
}

const MERGE_GAP: usize = 4;
const FULL_REPLACE_THRESHOLD: usize = 70;

fn compute_runs(prev: &[WireCell], cur: &[WireCell]) -> Vec<DiffRun> {
    debug_assert_eq!(prev.len(), cur.len());
    let n = cur.len();
    let mut runs: Vec<DiffRun> = Vec::new();
    let mut changed_total = 0usize;
    let mut i = 0;
    while i < n {
        if prev[i] == cur[i] {
            i += 1;
            continue;
        }
        let start = i;
        let mut last_change = i + 1;
        let mut j = i + 1;
        while j < n {
            if prev[j] == cur[j] {
                if j - last_change >= MERGE_GAP {
                    break;
                }
            } else {
                last_change = j + 1;
            }
            j += 1;
        }
        let end = last_change;
        let run_cells: Vec<WireCell> = cur[start..end].to_vec();
        changed_total += run_cells.len();
        runs.push(DiffRun {
            start: start as u32,
            cells: run_cells,
        });
        i = end;
    }
    if n > 0 && (changed_total * 100 / n) > FULL_REPLACE_THRESHOLD {
        return vec![DiffRun {
            start: 0,
            cells: cur.to_vec(),
        }];
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_to_bits_packs_known_flags() {
        let m = Modifier::BOLD | Modifier::REVERSED;
        let bits = modifier_to_bits(m);
        assert_eq!(bits & ATTR_BOLD, ATTR_BOLD);
        assert_eq!(bits & ATTR_REVERSED, ATTR_REVERSED);
        assert_eq!(bits & ATTR_DIM, 0);
    }

    #[test]
    fn color_passthrough_rgb() {
        assert_eq!(
            color_to_rgba(Color::Rgb(1, 2, 3), false),
            pack_rgba_u8(1, 2, 3, 0xff),
        );
    }

    #[test]
    fn compute_runs_empty_when_unchanged() {
        let prev = vec![WireCell::default(); 10];
        let cur = vec![WireCell::default(); 10];
        let runs = compute_runs(&prev, &cur);
        assert!(runs.is_empty());
    }

    #[test]
    fn compute_runs_single_block_when_one_cell_changes() {
        let prev = vec![WireCell::default(); 10];
        let mut cur = prev.clone();
        cur[5].ch = b'X' as u32;
        let runs = compute_runs(&prev, &cur);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].start, 5);
        assert_eq!(runs[0].cells.len(), 1);
    }

    #[test]
    fn compute_runs_merges_nearby_changes() {
        let prev = vec![WireCell::default(); 10];
        let mut cur = prev.clone();
        cur[2].ch = b'A' as u32;
        cur[5].ch = b'B' as u32;
        // Gap of 2 between [2] and [5] is < MERGE_GAP=4, so one run.
        let runs = compute_runs(&prev, &cur);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].start, 2);
        assert_eq!(runs[0].cells.len(), 4);
    }

    #[test]
    fn compute_runs_full_replace_when_most_changes() {
        let prev = vec![WireCell::default(); 10];
        let mut cur = prev.clone();
        for c in cur.iter_mut().take(8) {
            c.ch = b'Z' as u32;
        }
        let runs = compute_runs(&prev, &cur);
        // 80% changed → single full-grid run.
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].start, 0);
        assert_eq!(runs[0].cells.len(), 10);
    }
}
