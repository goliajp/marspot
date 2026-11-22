//! The regions a writer publishes beside the grid: cluster text and
//! links, and the window title.

use super::{ClusterTable, ClusterTableWriter, GridShmWriter, Identity, TitleWriter, names};

impl GridShmWriter {
    /// The cluster table in the region beside the grid: until the
    /// first frame is published, what the session's previous image
    /// published with its last one.
    pub fn published_clusters(&self) -> Option<ClusterTable> {
        self.clusters.as_ref().map(ClusterTableWriter::table)
    }

    /// Take over the side regions `left` publishes through: this
    /// writer replaces it, and [`Self::attach_side_regions`] then
    /// carries what they hold to wherever this one publishes.
    pub fn inherit_side_regions(&mut self, left: &mut Self) {
        self.clusters = left.clusters.take();
        self.title = left.title.take();
    }

    /// Publish the text of each frame's clusters and the window title
    /// from now on, in the regions named after the grid region `grid`
    /// -- see [`clusters`] and [`title`].  What it published beside the
    /// region it is leaving goes along.  Without the first a reader
    /// draws a cluster cell's pool index and nothing reads the second,
    /// so a failure is logged and the session runs without.
    pub fn attach_side_regions(&mut self, grid: &std::ffi::CStr, owner: Identity) {
        use names::{Side, side_name};
        // what this writer published beside the region it is leaving
        let (left_clusters, left_title) = (self.clusters.take(), self.title.take());
        let Some((clusters, title)) = side_name(grid, Side::Clusters).zip(side_name(grid, Side::Title))
        else {
            return;
        };
        match ClusterTableWriter::open(&clusters, owner) {
            Ok(mut w) => {
                if let Some(left) = &left_clusters {
                    w.carry_from(left);
                }
                self.clusters = Some(w);
                self.last_publish_hash = 0;
            }
            Err(e) => crate::lx_warn!(
                "grid_shm.clusters.open_failed",
                &format!("{e}"),
                session_id = owner.session
            ),
        }
        match TitleWriter::open(&title, owner) {
            Ok(mut w) => {
                if let Some(left) = &left_title {
                    w.carry_from(left);
                }
                self.title = Some(w);
            }
            Err(e) => {
                crate::lx_warn!("grid_shm.title.open_failed", &format!("{e}"), session_id = owner.session)
            }
        }
    }

    /// Publish `title` when it changed. Apart from the frame: a title
    /// can change while every cell stays the same.
    pub fn publish_title(&mut self, title: &str) {
        if let Some(t) = self.title.as_mut() {
            t.update(title);
        }
    }
}
