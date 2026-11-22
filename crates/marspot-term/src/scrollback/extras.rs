//! What history keeps beside a line's cells: the text of its clusters
//! and the targets of its links.

use super::Scrollback;

impl Scrollback {
    /// True when the scrollback itself keeps the text of clusters that
    /// have scrolled off, so the grid does not have to mirror it.
    ///
    /// The in-RAM variant has nowhere to put it and mirrors it in
    /// `Grid::sb_clusters`, bounded by the ring. The file-backed one
    /// cannot mirror it: its history outlives the process, and a
    /// mirror of a million lines is a million allocations that are
    /// never freed.
    pub fn keeps_cluster_text(&self) -> bool {
        matches!(self, Self::File(_))
    }

    /// The clusters on scrollback line `idx`, 0 = oldest, appended to
    /// `out`.  Memory answers nothing -- ask
    /// [`Self::keeps_cluster_text`] before believing a `false`.
    pub fn clusters_at(&self, idx: usize, out: &mut Vec<(u16, String)>) -> bool {
        match self {
            Self::Memory(_) => false,
            Self::File(f) => f.clusters_at(idx, out),
        }
    }

    /// File the clusters of the line most recently pushed.  A no-op for
    /// the in-RAM variant, whose clusters live in the grid's mirror.
    pub fn cluster_last_line(&mut self, clusters: &[(u16, String)]) {
        if let Self::File(f) = self {
            f.cluster_last_line(clusters);
        }
    }

    /// True when the scrollback itself keeps the links of lines that
    /// have scrolled off; the in-RAM variant leaves them to
    /// `Grid::sb_links`, for the reasons [`Self::keeps_cluster_text`]
    /// gives.
    pub fn keeps_link_text(&self) -> bool {
        matches!(self, Self::File(_))
    }

    /// The links on scrollback line `idx`, 0 = oldest, appended to `out`.
    pub fn links_at(&self, idx: usize, out: &mut Vec<crate::grid::RowLink>) -> bool {
        match self {
            Self::Memory(_) => false,
            Self::File(f) => f.links_at(idx, out),
        }
    }

    /// File the links of the line most recently pushed.  A no-op for
    /// the in-RAM variant.
    pub fn link_last_line(&mut self, links: &[crate::grid::RowLink]) {
        if let Self::File(f) = self {
            f.link_last_line(links);
        }
    }
}
