//! Filling L2's mirror of an L3 grid from the frame L3 published.

use std::time::{Duration, Instant};

use super::L3Conn;
use crate::grid::Grid;
use crate::grid_shm::{ClusterTableReader, GridSnapshot};

/// How often to look again for a session's cluster region while it has
/// none: the L3 creates it after it starts, and an L3 from before
/// clusters were published never does.
const RETRY_CLUSTERS_EVERY: Duration = Duration::from_secs(1);

impl L3Conn {
    /// Copy the latest published frame into the mirror: cells, per-row
    /// wrap flags, cursor, and the text of its clusters. `None` when L3
    /// has not published yet.
    pub(super) fn read_into_mirror(&mut self) -> Option<GridSnapshot> {
        if !self.reader.has_clusters()
            && self.clusters_tried_at.is_none_or(|t| t.elapsed() >= RETRY_CLUSTERS_EVERY)
        {
            self.clusters_tried_at = Some(Instant::now());
            if let Ok(r) = ClusterTableReader::open(self.session_id) {
                self.reader.attach_clusters(r);
            }
        }
        let snap = self.reader.read_with_clusters(
            &mut self.scratch,
            &mut self.scratch_wrapped,
            &mut self.cluster_table,
        )?;
        // Reshape the mirror if L3's geometry changed under us.
        if self.grid.cols() != snap.cols || self.grid.rows() != snap.rows {
            self.grid = Grid::new(snap.cols, snap.rows);
        }
        for row in 0..snap.rows {
            let base = row as usize * snap.cols as usize;
            for col in 0..snap.cols {
                self.grid.set_cell(col, row, self.scratch[base + col as usize]);
            }
        }
        // Per-row DECAWM continuation flags — without this the L2-side
        // link scanner can't tell a soft-wrapped row from a real
        // newline, breaking URL/path detection that overflows a row.
        for row in 0..snap.rows {
            let flag = self.scratch_wrapped.get(row as usize).copied().unwrap_or(false);
            self.grid.set_row_wrapped(row, flag);
        }
        // The cells carry L3's pool indices; this pool is the mirror's.
        self.grid.mirror_clusters(self.cluster_table.iter());
        self.grid.set_cursor(snap.cursor_col, snap.cursor_row);
        Some(snap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid_shm::{GridShmReader, GridShmWriter};
    use crate::terminal::Terminal;
    use std::os::unix::net::UnixStream;

    const FLAG: &str = "\u{1f1ef}\u{1f1f5}";
    const FAMILY: &str = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";

    fn conn_on(writer: &GridShmWriter, id: u64) -> L3Conn {
        let fd = unsafe { libc::dup(writer.fd()) };
        assert!(fd >= 0, "dup of the shm fd");
        let reader = GridShmReader::from_fd(fd).expect("shm reader");
        let (control, _peer) = UnixStream::pair().expect("socketpair");
        let (_tx, selection_rx) = std::sync::mpsc::channel();
        // our own pid: `reattach` never signals it
        L3Conn::reattach(std::process::id() as i32, control, reader, selection_rx, id)
    }

    fn session_id() -> u64 {
        0xC2_0000_0000 + std::process::id() as u64
    }

    /// The mirror used to keep L3's pool indices and an empty pool of
    /// its own, so a flag or a ZWJ family was drawn as a private-use
    /// codepoint.
    #[test]
    fn the_mirror_holds_the_text_of_each_cluster_l3_published() {
        let id = session_id();
        let mut writer = GridShmWriter::create(20, 3).expect("shm region");
        writer.attach_clusters(id);
        let mut term = Terminal::new(20, 3);
        term.feed(format!("{FLAG} {FAMILY}").as_bytes());
        writer.publish(term.grid(), 0, 0);

        let mut conn = conn_on(&writer, id);
        assert!(conn.poll(), "the published frame reaches the mirror");
        let flag = conn.grid.cell(0, 0);
        let family = conn.grid.cell(3, 0);
        crate::grid_shm::clusters::delete(id);
        assert_eq!(conn.grid.cluster_text(&flag), Some(FLAG));
        assert_eq!(conn.grid.cluster_text(&family), Some(FAMILY));
    }

    /// An L3 from before clusters were published sends cells whose
    /// indices mean nothing here. They must not be read as whatever
    /// this pool holds at that slot.
    #[test]
    fn a_cluster_from_an_l3_without_a_table_reads_as_unknown() {
        let mut writer = GridShmWriter::create(20, 3).expect("shm region");
        let mut term = Terminal::new(20, 3);
        term.feed(FLAG.as_bytes());
        writer.publish(term.grid(), 0, 0);

        let mut conn = conn_on(&writer, session_id() + 1);
        assert!(conn.poll());
        let cell = conn.grid.cell(0, 0);
        assert!(!cell.is_cluster());
        assert_eq!(cell.ch, crate::grid::UNKNOWN_CLUSTER);
    }

    /// What the renderer would draw for `grid`: its mono and colour
    /// glyph instances, built the way a frame is, with a fresh atlas.
    fn drawn(grid: &Grid) -> Option<(Vec<crate::render_metal::GlyphInstance>, Vec<crate::render_metal::GlyphInstance>)> {
        use crate::font_cache::FontCache;
        use crate::layout::Layout;
        use crate::render::SessionView;
        use crate::render_metal::{new_atlas, system_default_device, CellInstance, GlyphInstance};
        let device = system_default_device().ok()?;
        let mut font = FontCache::build().expect("font");
        let mut atlas = new_atlas(&device, 1024, 1024, false, font.font_table()).map(|(a, _)| a).expect("atlas");
        let mut color_atlas =
            new_atlas(&device, 1024, 1024, true, font.font_table()).map(|(a, _)| a).expect("colour atlas");
        let (cols, rows) = (grid.cols(), grid.rows());
        let layout = Layout::build(
            font.cell_w * cols as f64,
            font.cell_h * rows as f64,
            0.0,
            0.0,
            0.0,
            1,
            1,
            font.cell_w,
            font.cell_h,
        );
        let view = SessionView {
            grid,
            view_offset: 0,
            cursor_visible: false,
            focused: true,
            title: "",
            selection: None,
            ime_preedit: "",
            update_pending: false,
            dormant: false,
            recede: 0,
            scrim: 0.0,
            right_badge: "",
            agent_tui: false,
            cwd: "",
            top_fixed_h_cells: 0,
            bot_fixed_h_cells: 0,
            highlight_spans: &[],
            search_overlay: None,
            seq: 0,
        };
        let mut cells: Vec<CellInstance> = Vec::new();
        let mut glyphs: Vec<GlyphInstance> = Vec::new();
        let mut color_glyphs: Vec<GlyphInstance> = Vec::new();
        crate::frame_build::frame::build_instances(
            &layout,
            std::slice::from_ref(&view),
            &[],
            0,
            true,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            &mut font,
            &mut atlas,
            &mut color_atlas,
            &mut cells,
            &mut glyphs,
            &mut color_glyphs,
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
        );
        Some((glyphs, color_glyphs))
    }

    /// An L3 pane draws a cluster exactly as an in-process pane does:
    /// the same glyphs, from the same atlas entries, in the same places.
    /// Without the cluster table it does not, which is what this test
    /// would have caught.
    #[test]
    fn an_l3_pane_draws_clusters_as_the_terminal_holding_them_does() {
        const ACCENT: &str = "e\u{301}";
        let text = format!("{FLAG} {FAMILY} {ACCENT}");
        let mut term = Terminal::new(20, 3);
        term.feed(text.as_bytes());
        let Some(direct) = drawn(term.grid()) else {
            eprintln!("skip (no Metal)");
            return;
        };
        assert!(direct.0.len() + direct.1.len() >= 3, "the fixture must draw its three clusters");

        let id = session_id() + 2;
        let mut with_table = GridShmWriter::create(20, 3).expect("shm region");
        with_table.attach_clusters(id);
        with_table.publish(term.grid(), 0, 0);
        let mut conn = conn_on(&with_table, id);
        assert!(conn.poll());
        crate::grid_shm::clusters::delete(id);
        assert_eq!(drawn(&conn.grid), Some(direct.clone()), "the mirror draws what the terminal draws");

        let mut without = GridShmWriter::create(20, 3).expect("shm region");
        without.publish(term.grid(), 0, 0);
        let mut old = conn_on(&without, session_id() + 3);
        assert!(old.poll());
        assert_ne!(drawn(&old.grid), Some(direct), "without the table the clusters are lost");
    }
}
