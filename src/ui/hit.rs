//! フレームレイアウトのヒットマップ
//!
//! レンダラーは描画のその場で「この矩形はこの意味」を登録する。折返し行・
//! フィルタ済みインデックス・中央寄せオフセットなどの逆写像を入力側で
//! 複製しないため、登録時点で解決済みの値(実インデックス・論理行)を持たせる。
//!
//! 判定は登録の逆順(後に描いたもの=最上位が勝つ)。オーバーレイは描画順で
//! 後に登録されるため、z-order が自然に表現される。

use ratatui::layout::{Position, Rect};

/// リスト画面の識別子(HitTarget::ListRow / PaneKind::List が指す先)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListKind {
    PrList,
    FileList,
    ChecksList,
    IssueList,
    CockpitMenu,
    AiRallyLog,
    CommentList,
    IssueCommentList,
    BrowseTree,
    BrowseGraph,
    GitOpsTree,
    GitOpsCommits,
    IssueLinkedPrs,
    // オーバーレイ内のリスト
    SymbolPopup,
    BrowseOutline,
    BrowseSymbolSearch,
    BrowsePrChoice,
    BrowseLineDiscussion,
}

/// スクロール/フォーカス対象となるペインの識別子
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneKind {
    List(ListKind),
    Diff,
    GitOpsDiff,
    PrDescription,
    Help,
    BrowseFile,
    IssueBody,
    TextArea,
    ShellOutput,
    ModuleGraph,
    /// 差分ビューのインラインコメントパネル
    CommentPanel,
    /// Git 破壊的操作確認のシミュレーションプレビュー
    GitOpsPreview,
}

/// ドラッグでリサイズ可能なスプリット境界
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitKind {
    LeftPanel,
}

/// クリック可能なタブ群
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabGroup {
    CommentTabs,
    HelpTabs,
    GraphDirection,
    IssueDetailFocus,
}

/// 確認ダイアログの識別子
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogKind {
    GitOpsConfirm,
    RallyPermission,
    RallyPost,
    ApproveEmptyBody,
}

/// ヒット判定の対象。描画時に解決済みの値を保持する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitTarget {
    /// リストの 1 行。`row` は選択フィールドが使う表示空間の位置
    /// (フィルタ中は filter.selected、ツリーは行番号)。`index` は
    /// フィルタ変換等を済ませた実インデックス(非フィルタ時は row と同値)。
    ListRow {
        list: ListKind,
        row: usize,
        index: usize,
    },
    /// スクロールテキスト/差分の 1 行。`line` は論理行(折返し解決済み)。
    ContentLine { pane: PaneKind, line: usize },
    /// ペイン全体(行より下に登録し、行以外の余白のホイール/フォーカスを拾う)
    Pane { pane: PaneKind },
    /// スプリット境界(ドラッグでリサイズ)。body_x/body_width は分割対象領域の
    /// 水平範囲で、ドラッグ中の x 座標→パーセント換算に使う。
    SplitDivider {
        split: SplitKind,
        body_x: u16,
        body_width: u16,
    },
    /// クリック可能なタブ
    Tab { group: TabGroup, index: usize },
    /// ダイアログの応答ボタン(yes=true が confirm_yes 相当)
    DialogButton { dialog: DialogKind, yes: bool },
    /// オーバーレイの背面。dismiss=true なら外側クリックで閉じる。
    Backdrop { dismiss: bool },
    /// オーバーレイのコンテンツ面。内部の非操作領域のクリックを消費する。
    OverlaySurface,
}

/// 1 フレーム分のヒット領域。描画のたびに clear→登録し直す(容量は維持)。
#[derive(Debug, Default)]
pub struct HitMap {
    regions: Vec<(Rect, HitTarget)>,
}

impl HitMap {
    pub fn clear(&mut self) {
        self.regions.clear();
    }

    pub fn push(&mut self, rect: Rect, target: HitTarget) {
        if rect.width > 0 && rect.height > 0 {
            self.regions.push((rect, target));
        }
    }

    /// 座標に重なる最上位(=最後に登録された)ターゲットを返す
    pub fn hit(&self, x: u16, y: u16) -> Option<HitTarget> {
        let pos = Position { x, y };
        self.regions
            .iter()
            .rev()
            .find(|(rect, _)| rect.contains(pos))
            .map(|(_, target)| *target)
    }

    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.regions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    #[test]
    fn test_hit_returns_none_for_empty_map() {
        let map = HitMap::default();
        assert_eq!(map.hit(0, 0), None);
    }

    #[test]
    fn test_hit_returns_none_outside_all_regions() {
        let mut map = HitMap::default();
        map.push(
            rect(0, 0, 10, 5),
            HitTarget::Pane {
                pane: PaneKind::Diff,
            },
        );
        assert_eq!(map.hit(10, 0), None, "右端は排他境界");
        assert_eq!(map.hit(0, 5), None, "下端は排他境界");
    }

    #[test]
    fn test_hit_last_registered_wins() {
        let mut map = HitMap::default();
        map.push(
            rect(0, 0, 20, 20),
            HitTarget::Pane {
                pane: PaneKind::Diff,
            },
        );
        map.push(
            rect(5, 5, 5, 1),
            HitTarget::ListRow {
                list: ListKind::PrList,
                row: 3,
                index: 3,
            },
        );
        assert_eq!(
            map.hit(6, 5),
            Some(HitTarget::ListRow {
                list: ListKind::PrList,
                row: 3,
                index: 3
            })
        );
        assert_eq!(
            map.hit(1, 1),
            Some(HitTarget::Pane {
                pane: PaneKind::Diff
            })
        );
    }

    #[test]
    fn test_hit_overlay_z_order() {
        let mut map = HitMap::default();
        map.push(
            rect(0, 0, 40, 20),
            HitTarget::ListRow {
                list: ListKind::FileList,
                row: 0,
                index: 0,
            },
        );
        map.push(rect(0, 0, 40, 20), HitTarget::Backdrop { dismiss: true });
        map.push(rect(10, 5, 20, 10), HitTarget::OverlaySurface);
        map.push(
            rect(11, 6, 18, 1),
            HitTarget::ListRow {
                list: ListKind::BrowseTree,
                row: 2,
                index: 2,
            },
        );

        assert_eq!(
            map.hit(0, 0),
            Some(HitTarget::Backdrop { dismiss: true }),
            "オーバーレイ外は Backdrop(下のリストには届かない)"
        );
        assert_eq!(
            map.hit(15, 8),
            Some(HitTarget::OverlaySurface),
            "オーバーレイ内の余白は Surface が消費"
        );
        assert_eq!(
            map.hit(12, 6),
            Some(HitTarget::ListRow {
                list: ListKind::BrowseTree,
                row: 2,
                index: 2
            })
        );
    }

    #[test]
    fn test_zero_size_rect_is_not_registered() {
        let mut map = HitMap::default();
        map.push(rect(0, 0, 0, 5), HitTarget::OverlaySurface);
        map.push(rect(0, 0, 5, 0), HitTarget::OverlaySurface);
        assert_eq!(map.len(), 0);
    }

    #[test]
    fn test_clear_keeps_capacity() {
        let mut map = HitMap::default();
        for i in 0..100 {
            map.push(rect(0, i, 10, 1), HitTarget::OverlaySurface);
        }
        map.clear();
        assert_eq!(map.len(), 0);
        assert_eq!(map.hit(0, 0), None);
    }
}
