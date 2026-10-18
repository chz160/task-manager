//! Keeps Slint table models in sync with each refresh while touching as few rows as possible:
//! every touched row costs a repaint, and most of a thousand processes do not change.

use slint::{Model, ModelRc, SharedString, StandardListViewItem, VecModel};
use std::rc::Rc;

pub type Row = ModelRc<StandardListViewItem>;

/// The cells of one row; kept so that a changed value rewrites its own cell and the other
/// cells of the row are not laid out and painted again.
type Cells = Rc<VecModel<StandardListViewItem>>;

fn item(text: &str) -> StandardListViewItem {
    let mut i = StandardListViewItem::default();
    i.text = SharedString::from(text);
    i
}

fn new_cells<const N: usize>(cells: &[String; N]) -> Cells {
    Rc::new(VecModel::from(
        cells.iter().map(|c| item(c)).collect::<Vec<_>>(),
    ))
}

/// Rewrites the cells of `model` that differ between `old` and `new`.
fn update_cells<const N: usize>(model: &Cells, old: &[String; N], new: &[String; N]) {
    for (c, (o, n)) in old.iter().zip(new).enumerate() {
        if o != n {
            model.set_row_data(c, item(n));
        }
    }
}

/// Marks which entries of `seq` belong to a longest strictly increasing subsequence.
fn lis_members(seq: &[usize]) -> Vec<bool> {
    // tails[k] = index in `seq` of the smallest tail of an increasing run of length k + 1.
    let mut tails: Vec<usize> = Vec::new();
    let mut prev: Vec<Option<usize>> = vec![None; seq.len()];
    for (i, &v) in seq.iter().enumerate() {
        let k = tails.partition_point(|&t| seq[t] < v);
        prev[i] = k.checked_sub(1).map(|k| tails[k]);
        if k == tails.len() {
            tails.push(i);
        } else {
            tails[k] = i;
        }
    }
    let mut member = vec![false; seq.len()];
    let mut cur = tails.last().copied();
    while let Some(i) = cur {
        member[i] = true;
        cur = prev[i];
    }
    member
}

/// Rows identified by a key (the PID). A row that changes rank is moved on its own instead of
/// rewriting every row between its old and new position.
pub struct KeyedRows<const N: usize> {
    keys: Vec<i32>,
    cells: Vec<[String; N]>,
    models: Vec<Cells>,
}

impl<const N: usize> KeyedRows<N> {
    pub fn new() -> Self {
        Self {
            keys: Vec::new(),
            cells: Vec::new(),
            models: Vec::new(),
        }
    }

    pub fn sync(&mut self, model: &VecModel<Row>, rows: Vec<(i32, [String; N])>) {
        // Rows that stay put are those on a longest run that is already in the right
        // relative order; everything else is removed and re-inserted at its new rank.
        let old_pos: std::collections::HashMap<i32, usize> =
            self.keys.iter().enumerate().map(|(i, k)| (*k, i)).collect();
        let kept_new_idx: Vec<usize> = (0..rows.len())
            .filter(|&i| old_pos.contains_key(&rows[i].0))
            .collect();
        let seq: Vec<usize> = kept_new_idx.iter().map(|&i| old_pos[&rows[i].0]).collect();
        let member = lis_members(&seq);
        let mut stay = vec![false; self.keys.len()];
        for (m, &is) in member.iter().enumerate() {
            stay[seq[m]] = is;
        }
        for i in (0..self.keys.len()).rev() {
            if !stay[i] {
                self.keys.remove(i);
                self.cells.remove(i);
                self.models.remove(i);
                model.remove(i);
            }
        }
        for (i, (key, cells)) in rows.into_iter().enumerate() {
            if self.keys.get(i) == Some(&key) {
                if self.cells[i] != cells {
                    update_cells(&self.models[i], &self.cells[i], &cells);
                    self.cells[i] = cells;
                }
            } else {
                let inner = new_cells(&cells);
                model.insert(i, ModelRc::from(inner.clone()));
                self.keys.insert(i, key);
                self.cells.insert(i, cells);
                self.models.insert(i, inner);
            }
        }
    }
}

/// Positional variant for small tables with no stable key: rewrites only rows whose text changed.
pub struct RowCache<const N: usize> {
    cells: Vec<[String; N]>,
    models: Vec<Cells>,
}

impl<const N: usize> RowCache<N> {
    pub fn new() -> Self {
        Self {
            cells: Vec::new(),
            models: Vec::new(),
        }
    }

    pub fn sync(&mut self, model: &VecModel<Row>, rows: Vec<[String; N]>) {
        for (i, cells) in rows.iter().enumerate() {
            match self.cells.get(i) {
                Some(old) if old == cells => {}
                Some(old) => update_cells(&self.models[i], old, cells),
                None => {
                    let inner = new_cells(cells);
                    model.push(ModelRc::from(inner.clone()));
                    self.models.push(inner);
                }
            }
        }
        while model.row_count() > rows.len() {
            model.remove(model.row_count() - 1);
        }
        self.models.truncate(rows.len());
        self.cells = rows;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(keys: &[i32]) -> Vec<(i32, [String; 1])> {
        keys.iter().map(|k| (*k, [format!("p{k}")])).collect()
    }

    fn model_keys(model: &VecModel<Row>) -> Vec<String> {
        (0..model.row_count())
            .map(|i| {
                model
                    .row_data(i)
                    .unwrap()
                    .row_data(0)
                    .unwrap()
                    .text
                    .to_string()
            })
            .collect()
    }

    fn expect(keys: &[i32]) -> Vec<String> {
        keys.iter().map(|k| format!("p{k}")).collect()
    }

    #[test]
    fn lis_marks_a_longest_increasing_run() {
        let m = lis_members(&[3, 1, 2, 5, 4, 6]);
        assert_eq!(m.iter().filter(|b| **b).count(), 4);
        assert_eq!(lis_members(&[]), Vec::<bool>::new());
    }

    #[test]
    fn model_always_matches_target_order() {
        let model = VecModel::<Row>::default();
        let mut k = KeyedRows::<1>::new();
        let steps: [&[i32]; 7] = [
            &[5, 4, 3, 2, 1],
            &[3, 5, 4, 2, 1],
            &[5, 4, 3, 2, 1, 9],
            &[9, 1, 2, 3, 4, 5],
            &[2, 4],
            &[],
            &[7, 8, 9],
        ];
        for keys in steps {
            k.sync(&model, rows(keys));
            assert_eq!(model_keys(&model), expect(keys));
        }
    }

    #[test]
    fn two_rows_dropping_far_do_not_rewrite_the_rest() {
        let model = VecModel::<Row>::default();
        let mut k = KeyedRows::<1>::new();
        let before: Vec<i32> = (0..1000).collect();
        k.sync(&model, rows(&before));
        // Rows 1 and 2 sink to the bottom; everything between keeps its order.
        let mut after: Vec<i32> = before
            .iter()
            .copied()
            .filter(|x| *x > 2 || *x == 0)
            .collect();
        after.extend([1, 2]);
        k.sync(&model, rows(&after));
        assert_eq!(model_keys(&model), expect(&after));
        assert_eq!(k.keys, after);
    }

    #[test]
    fn changed_cells_are_rewritten_in_place() {
        let model = VecModel::<Row>::default();
        let mut k = KeyedRows::<1>::new();
        k.sync(&model, rows(&[1, 2]));
        k.sync(&model, vec![(1, ["p1".into()]), (2, ["changed".into()])]);
        assert_eq!(model_keys(&model), ["p1", "changed"]);
    }

    #[test]
    fn row_cache_updates_and_truncates() {
        let model = VecModel::<Row>::default();
        let mut c = RowCache::<1>::new();
        c.sync(&model, vec![["a".into()], ["b".into()], ["c".into()]]);
        c.sync(&model, vec![["a".into()], ["x".into()]]);
        assert_eq!(model_keys(&model), ["a", "x"]);
    }
}
