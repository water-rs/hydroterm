// Reflow + SIGWINCH-redraw probe: does alacritty's grid reflow leave stale
// prompt fragments after a shrink, once a bash-style redraw rewrites the
// prompt at the new width?
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::{Config, Term, cell::Cell};
use alacritty_terminal::vte::ansi::Processor;

struct Sz(usize, usize);
impl Dimensions for Sz {
    fn total_lines(&self) -> usize {
        self.0
    }
    fn screen_lines(&self) -> usize {
        self.0
    }
    fn columns(&self) -> usize {
        self.1
    }
}

fn dump(term: &Term<VoidListener>, cols: usize) {
    let grid = term.grid();
    let lines = grid.screen_lines();
    for l in 0..lines {
        let mut s = String::new();
        for c in 0..cols {
            let cell: &Cell = &grid[alacritty_terminal::index::Line(l as i32)]
                [alacritty_terminal::index::Column(c)];
            s.push(if cell.c == '\0' { ' ' } else { cell.c });
        }
        println!("{l:3}|{}|", s.trim_end());
    }
    let cur = &grid.cursor;
    println!(
        "cursor: line={} col={} pending_wrap={}",
        cur.point.line, cur.point.column, cur.input_needs_wrap
    );
}

fn feed(term: &mut Term<VoidListener>, p: &mut Processor, bytes: &[u8]) {
    p.advance(term, bytes);
}

fn main() {
    let prompt = "ubuntu@devin-box:~/repos/hydroterm$ ";
    // Start at 48 columns, 8 lines of history + the prompt (no typed text).
    let mut term = Term::new(Config::default(), &Sz(24, 48), VoidListener);
    let mut p: Processor = Processor::new();
    let mut pre = String::new();
    for i in 0..8 {
        pre.push_str(&format!("history line {i}\r\n"));
    }
    feed(&mut term, &mut p, pre.as_bytes());
    feed(&mut term, &mut p, prompt.as_bytes());
    println!("=== 48 cols (start) ===");
    dump(&term, 48);

    // Real bytes captured from bash 5.1 SIGWINCH redraws (pty capture).
    let step31 = b"\r                              \r<~/repos/hydroterm$ \r                              \r<~/repos/hydroterm$ ";
    let step14 = b"\r             \r<oterm$ ";
    for (cols, bytes) in [(31usize, &step31[..]), (14usize, &step14[..])] {
        term.resize(Sz(24, cols));
        // Mirror of hydroterm's clear_prompt_for_redraw at `redraw=last`
        // (what our bash integration emits): blank the cursor row only.
        {
            let grid = term.grid_mut();
            let cursor = grid.cursor.point.line.0;
            let template = grid.cursor.template.clone();
            for col in 0..cols {
                grid[Line(cursor)][Column(col)] = template.clone();
            }
            println!("  [cleared cursor row {cursor}]");
        }
        println!("=== after resize to {cols} (no redraw yet) ===");
        dump(&term, cols);
        feed(&mut term, &mut p, bytes);
        println!("=== after bash redraw at {cols} ===");
        dump(&term, cols);
    }
}
