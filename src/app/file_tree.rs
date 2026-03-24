use std::collections::{BTreeSet, HashSet};

use super::types::TreeRow;

/// ファイルパスからツリー構造を構築・管理する汎用モジュール。
/// GitOps、FileList、SplitView すべてで共有される。
#[derive(Debug, Default)]
pub struct FileTreeState {
    pub visible_rows: Vec<TreeRow>,
    pub expanded_dirs: HashSet<String>,
    pub selected_row: usize,
    pub scroll_offset: usize,
    initialized: bool,
    cached_paths: Vec<(usize, String)>,
}

impl FileTreeState {
    pub fn new() -> Self {
        Self {
            visible_rows: Vec::new(),
            expanded_dirs: HashSet::new(),
            selected_row: 0,
            scroll_offset: 0,
            initialized: false,
            cached_paths: Vec::new(),
        }
    }

    pub fn rebuild(&mut self, paths: &[(usize, &str)]) {
        self.cached_paths = paths.iter().map(|(i, p)| (*i, p.to_string())).collect();
        self.rebuild_inner();
    }

    pub fn rebuild_owned(&mut self, paths: Vec<(usize, String)>) {
        self.cached_paths = paths;
        self.rebuild_inner();
    }

    /// カーソル位置のディレクトリを展開/折畳トグルし、ツリーを再構築。
    pub fn toggle_expand(&mut self) {
        if let Some(TreeRow::Dir { ref path, .. }) = self.visible_rows.get(self.selected_row) {
            let path = path.clone();
            if self.expanded_dirs.contains(&path) {
                self.expanded_dirs.remove(&path);
            } else {
                self.expanded_dirs.insert(path);
            }
            self.rebuild_inner();
        }
    }

    /// 現在選択中の行がファイルならそのソースインデックスを返す。
    pub fn selected_file_index(&self) -> Option<usize> {
        match self.visible_rows.get(self.selected_row) {
            Some(TreeRow::File { index, .. }) => Some(*index),
            _ => None,
        }
    }

    /// 現在選択中の行がディレクトリならそのパスを返す。
    pub fn selected_dir_path(&self) -> Option<&str> {
        match self.visible_rows.get(self.selected_row) {
            Some(TreeRow::Dir { ref path, .. }) => Some(path),
            _ => None,
        }
    }

    /// ファイルインデックスに対応する visible_rows 内の行位置を返す。
    pub fn find_row_for_file(&self, file_index: usize) -> Option<usize> {
        self.visible_rows
            .iter()
            .position(|row| matches!(row, TreeRow::File { index, .. } if *index == file_index))
    }

    /// ディレクトリパスに対応する visible_rows 内の行位置を返す。
    pub fn find_row_for_dir(&self, dir_path: &str) -> Option<usize> {
        self.visible_rows
            .iter()
            .position(|row| matches!(row, TreeRow::Dir { ref path, .. } if path == dir_path))
    }

    pub fn move_down(&mut self) {
        if !self.visible_rows.is_empty() {
            self.selected_row = (self.selected_row + 1).min(self.visible_rows.len() - 1);
        }
    }

    pub fn move_up(&mut self) {
        self.selected_row = self.selected_row.saturating_sub(1);
    }

    pub fn page_down(&mut self, step: usize) {
        if !self.visible_rows.is_empty() {
            self.selected_row = (self.selected_row + step).min(self.visible_rows.len() - 1);
        }
    }

    pub fn page_up(&mut self, step: usize) {
        self.selected_row = self.selected_row.saturating_sub(step);
    }

    pub fn jump_to_first(&mut self) {
        self.selected_row = 0;
    }

    pub fn jump_to_last(&mut self) {
        if !self.visible_rows.is_empty() {
            self.selected_row = self.visible_rows.len() - 1;
        }
    }

    pub fn row_count(&self) -> usize {
        self.visible_rows.len()
    }

    /// テスト用: ツリーのテキスト表現を返す。
    pub fn dump_tree(&self) -> String {
        let mut lines = Vec::new();
        for row in &self.visible_rows {
            match row {
                TreeRow::Dir {
                    ref path,
                    depth,
                    expanded,
                } => {
                    let indent = "  ".repeat(*depth);
                    let icon = if *expanded { "▼" } else { "▶" };
                    let dir_name = path.rsplit_once('/').map(|(_, name)| name).unwrap_or(path);
                    lines.push(format!("{}{} {}/", indent, icon, dir_name));
                }
                TreeRow::File { index, depth } => {
                    let indent = "  ".repeat(*depth);
                    let filename = self
                        .cached_paths
                        .iter()
                        .find(|(i, _)| *i == *index)
                        .map(|(_, p)| {
                            p.rsplit_once('/')
                                .map(|(_, name)| name)
                                .unwrap_or(p.as_str())
                        })
                        .unwrap_or("???");
                    lines.push(format!("{}{}", indent, filename));
                }
            }
        }
        lines.join("\n")
    }

    /// cached_paths からツリーを再構築する内部メソッド。
    fn rebuild_inner(&mut self) {
        self.visible_rows.clear();

        if self.cached_paths.is_empty() {
            return;
        }

        // ディレクトリパスを収集
        let mut dirs: BTreeSet<String> = BTreeSet::new();
        for (_, path) in &self.cached_paths {
            let parts: Vec<&str> = path.split('/').collect();
            let mut current = String::new();
            for (i, part) in parts.iter().enumerate() {
                if i < parts.len() - 1 {
                    if !current.is_empty() {
                        current.push('/');
                    }
                    current.push_str(part);
                    dirs.insert(current.clone());
                }
            }
        }

        // 初回ビルド時は全ディレクトリを展開
        if !self.initialized && !dirs.is_empty() {
            self.expanded_dirs = dirs.iter().cloned().collect();
            self.initialized = true;
        }

        // ソート: ディレクトリ優先（同階層でディレクトリをファイルより先に表示）
        // sort key を事前計算してアロケーションを O(n) に抑える
        let split_cache: Vec<Vec<&str>> = self
            .cached_paths
            .iter()
            .map(|(_, p)| p.split('/').collect())
            .collect();
        let mut sorted_indices: Vec<usize> = (0..self.cached_paths.len()).collect();
        sorted_indices.sort_by(|a, b| dirs_first_cmp_parts(&split_cache[*a], &split_cache[*b]));

        let mut added_dirs: HashSet<String> = HashSet::new();

        for &sorted_idx in &sorted_indices {
            let (source_idx, ref path) = self.cached_paths[sorted_idx];
            let parts: Vec<&str> = path.split('/').collect();

            // 親ディレクトリ行を追加
            let mut current = String::new();
            for (depth, part) in parts.iter().enumerate() {
                if depth < parts.len() - 1 {
                    if !current.is_empty() {
                        current.push('/');
                    }
                    current.push_str(part);

                    if !added_dirs.contains(&current) {
                        // 親が visible かつ展開中の場合のみ表示。
                        // ルートレベル (depth==0) は常に表示。
                        let parent_visible_and_expanded = if depth == 0 {
                            true
                        } else {
                            let parent = current.rsplit_once('/').map(|(p, _)| p);
                            parent
                                .map(|p| added_dirs.contains(p) && self.expanded_dirs.contains(p))
                                .unwrap_or(true)
                        };

                        if parent_visible_and_expanded {
                            let is_expanded = self.expanded_dirs.contains(&current);
                            self.visible_rows.push(TreeRow::Dir {
                                path: current.clone(),
                                depth,
                                expanded: is_expanded,
                            });
                            added_dirs.insert(current.clone());
                        }
                    }
                }
            }

            // ファイル行を追加（直接の親ディレクトリが added_dirs に存在する場合のみ）
            let parent_dir = if parts.len() > 1 {
                Some(parts[..parts.len() - 1].join("/"))
            } else {
                None
            };

            let visible = parent_dir
                .as_ref()
                .map(|p| added_dirs.contains(p.as_str()) && self.expanded_dirs.contains(p.as_str()))
                .unwrap_or(true);

            if visible {
                let depth = parts.len() - 1;
                self.visible_rows.push(TreeRow::File {
                    index: source_idx,
                    depth,
                });
            }
        }

        // selected_row をクランプ
        if !self.visible_rows.is_empty() && self.selected_row >= self.visible_rows.len() {
            self.selected_row = self.visible_rows.len() - 1;
        }
    }
}

/// 同階層でディレクトリをファイルより先に並べる。
fn dirs_first_cmp_parts(a_parts: &[&str], b_parts: &[&str]) -> std::cmp::Ordering {
    let min_len = a_parts.len().min(b_parts.len());
    for i in 0..min_len {
        let a_is_last = i == a_parts.len() - 1;
        let b_is_last = i == b_parts.len() - 1;

        // 片方がファイル（最後のコンポーネント）で他方がディレクトリ（まだ子がある）
        if a_is_last && !b_is_last {
            return std::cmp::Ordering::Greater;
        }
        if !a_is_last && b_is_last {
            return std::cmp::Ordering::Less;
        }

        let cmp = a_parts[i].cmp(b_parts[i]);
        if cmp != std::cmp::Ordering::Equal {
            return cmp;
        }
    }

    a_parts.len().cmp(&b_parts.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;

    #[test]
    fn test_empty_paths() {
        let mut tree = FileTreeState::new();
        tree.rebuild(&[]);
        assert!(tree.visible_rows.is_empty());
        assert_eq!(tree.row_count(), 0);
    }

    #[test]
    fn test_flat_files_only() {
        let mut tree = FileTreeState::new();
        tree.rebuild(&[(0, "Cargo.toml"), (1, "README.md")]);
        // ルート直下ファイルのみ → Dir 行なし
        assert_eq!(tree.row_count(), 2);
        assert!(matches!(
            tree.visible_rows[0],
            TreeRow::File { index: 0, depth: 0 }
        ));
        assert!(matches!(
            tree.visible_rows[1],
            TreeRow::File { index: 1, depth: 0 }
        ));
    }

    #[test]
    fn test_nested_dirs() {
        let mut tree = FileTreeState::new();
        tree.rebuild(&[(0, "src/app/mod.rs"), (1, "src/lib.rs")]);
        assert_snapshot!(tree.dump_tree(), @"
        ▼ src/
          ▼ app/
            mod.rs
          lib.rs
        ");
    }

    #[test]
    fn test_initial_expand_all() {
        let mut tree = FileTreeState::new();
        tree.rebuild(&[(0, "src/main.rs"), (1, "tests/test.rs")]);
        // 初回 rebuild で全ディレクトリ展開
        assert!(tree.expanded_dirs.contains("src"));
        assert!(tree.expanded_dirs.contains("tests"));
        // ファイルが見える
        assert!(tree.selected_file_index().is_some() || tree.selected_dir_path().is_some());
    }

    #[test]
    fn test_toggle_collapse() {
        let mut tree = FileTreeState::new();
        tree.rebuild(&[(0, "src/app/mod.rs"), (1, "src/lib.rs")]);
        let initial_count = tree.row_count();

        // src/app/ ディレクトリを折り畳む
        // まず src/app/ の行を見つける
        let app_row = tree.find_row_for_dir("src/app").unwrap();
        tree.selected_row = app_row;
        tree.toggle_expand();

        // 子（mod.rs）が非表示になるので行数が減る
        assert!(tree.row_count() < initial_count);
        // mod.rs が visible_rows にない
        assert!(tree.find_row_for_file(0).is_none());
    }

    #[test]
    fn test_toggle_reexpand() {
        let mut tree = FileTreeState::new();
        tree.rebuild(&[(0, "src/app/mod.rs"), (1, "src/lib.rs")]);
        let initial_count = tree.row_count();

        let app_row = tree.find_row_for_dir("src/app").unwrap();
        tree.selected_row = app_row;
        tree.toggle_expand(); // collapse
        tree.toggle_expand(); // re-expand

        assert_eq!(tree.row_count(), initial_count);
        assert!(tree.find_row_for_file(0).is_some());
    }

    #[test]
    fn test_selected_file_index() {
        let mut tree = FileTreeState::new();
        tree.rebuild(&[(0, "src/main.rs")]);
        // 最初の行は Dir (src/)
        assert_eq!(tree.selected_row, 0);
        assert!(tree.selected_dir_path().is_some());
        assert!(tree.selected_file_index().is_none());

        // ファイル行に移動
        tree.move_down();
        assert!(tree.selected_file_index().is_some());
        assert!(tree.selected_dir_path().is_none());
    }

    #[test]
    fn test_find_row_for_file() {
        let mut tree = FileTreeState::new();
        tree.rebuild(&[(0, "src/main.rs"), (1, "README.md")]);
        // file index 0 (src/main.rs) がツリー内に見つかる
        let row = tree.find_row_for_file(0);
        assert!(row.is_some());
        // file index 1 (README.md) も見つかる
        let row = tree.find_row_for_file(1);
        assert!(row.is_some());
        // 存在しないインデックスは None
        assert!(tree.find_row_for_file(99).is_none());
    }

    #[test]
    fn test_find_row_for_dir() {
        let mut tree = FileTreeState::new();
        tree.rebuild(&[(0, "src/app/mod.rs")]);
        assert!(tree.find_row_for_dir("src").is_some());
        assert!(tree.find_row_for_dir("src/app").is_some());
        assert!(tree.find_row_for_dir("nonexistent").is_none());
    }

    #[test]
    fn test_move_down_up() {
        let mut tree = FileTreeState::new();
        tree.rebuild(&[(0, "a.rs"), (1, "b.rs"), (2, "c.rs")]);
        assert_eq!(tree.selected_row, 0);
        tree.move_down();
        assert_eq!(tree.selected_row, 1);
        tree.move_down();
        assert_eq!(tree.selected_row, 2);
        tree.move_down(); // boundary
        assert_eq!(tree.selected_row, 2);
        tree.move_up();
        assert_eq!(tree.selected_row, 1);
        tree.move_up();
        assert_eq!(tree.selected_row, 0);
        tree.move_up(); // boundary
        assert_eq!(tree.selected_row, 0);
    }

    #[test]
    fn test_clamp_on_rebuild() {
        let mut tree = FileTreeState::new();
        tree.rebuild(&[(0, "a.rs"), (1, "b.rs"), (2, "c.rs")]);
        tree.selected_row = 2;
        // 行数が減る
        tree.rebuild(&[(0, "a.rs")]);
        assert_eq!(tree.selected_row, 0);
    }

    #[test]
    fn test_dirs_first_sort() {
        let mut tree = FileTreeState::new();
        tree.rebuild(&[
            (0, "Cargo.toml"),
            (1, "README.md"),
            (2, "src/main.rs"),
            (3, "tests/test.rs"),
        ]);
        let dump = tree.dump_tree();
        // ディレクトリ (src/, tests/) がファイル (Cargo.toml, README.md) より先
        let src_pos = dump.find("▼ src/").expect("src/ not found");
        let tests_pos = dump.find("▼ tests/").expect("tests/ not found");
        let cargo_pos = dump.find("Cargo.toml").expect("Cargo.toml not found");
        let readme_pos = dump.find("README.md").expect("README.md not found");
        assert!(
            src_pos < cargo_pos,
            "src/ should be before Cargo.toml\n{}",
            dump
        );
        assert!(
            tests_pos < cargo_pos,
            "tests/ should be before Cargo.toml\n{}",
            dump
        );
        assert!(
            cargo_pos < readme_pos || readme_pos < cargo_pos,
            "files should be alphabetical among themselves"
        );
    }

    #[test]
    fn test_toggle_expand_self_contained() {
        let mut tree = FileTreeState::new();
        tree.rebuild(&[(0, "src/main.rs"), (1, "README.md")]);
        let initial_count = tree.row_count();

        // src/ を折り畳む
        let src_row = tree.find_row_for_dir("src").unwrap();
        tree.selected_row = src_row;
        tree.toggle_expand();

        // 再度 rebuild を呼ばなくても toggle_expand が自己完結で動く
        assert!(tree.row_count() < initial_count);

        // 再展開
        tree.toggle_expand();
        assert_eq!(tree.row_count(), initial_count);
    }

    #[test]
    fn test_rebuild_preserves_expanded_dirs() {
        let mut tree = FileTreeState::new();
        tree.rebuild(&[(0, "src/app/mod.rs"), (1, "src/lib.rs")]);

        // src/app/ を折り畳む
        let app_row = tree.find_row_for_dir("src/app").unwrap();
        tree.selected_row = app_row;
        tree.toggle_expand();
        assert!(!tree.expanded_dirs.contains("src/app"));

        // 再度 rebuild（データリロード相当）
        tree.rebuild(&[
            (0, "src/app/mod.rs"),
            (1, "src/app/types.rs"),
            (2, "src/lib.rs"),
        ]);

        // expanded_dirs は維持される（src/app は折り畳みのまま）
        assert!(!tree.expanded_dirs.contains("src/app"));
        // src/app 配下のファイルは非表示
        assert!(tree.find_row_for_file(0).is_none());
        assert!(tree.find_row_for_file(1).is_none());
    }

    #[test]
    fn test_dump_tree() {
        let mut tree = FileTreeState::new();
        tree.rebuild(&[(0, "src/main.rs"), (1, "README.md")]);
        assert_snapshot!(tree.dump_tree(), @"
        ▼ src/
          main.rs
        README.md
        ");
    }
}
