//! The keyboard's way through the All view of search results: the top result, the songs
//! beside it, then each row of covers, in reading order.

/// A place in the All view the keyboard can select.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spot {
    /// The top result's card.
    Top,
    /// Song `n` of those beside the top result.
    Song(usize),
    /// Card `col` of row `row`, counting the rows of covers that show.
    Card { row: usize, col: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// Where an arrow key takes the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Move {
    To(Spot),
    /// Up from the first place: back to the search field.
    Field,
    Stay,
}

/// What the All view shows: whether it has a top result, how many songs, and how many cards
/// each row of covers has, in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Shape {
    pub top: bool,
    pub songs: usize,
    pub rows: Vec<usize>,
}

impl Shape {
    /// Whether the view has `spot`.
    pub fn holds(&self, spot: Spot) -> bool {
        match spot {
            Spot::Top => self.top,
            Spot::Song(i) => i < self.songs,
            Spot::Card { row, col } => self.rows.get(row).is_some_and(|&n| col < n),
        }
    }

    /// The first place in reading order.
    fn first(&self) -> Option<Spot> {
        if self.top {
            Some(Spot::Top)
        } else if self.songs > 0 {
            Some(Spot::Song(0))
        } else {
            self.card_in(0..self.rows.len(), 0)
        }
    }

    /// Column `col`, or the last one if the row is shorter, of the first row in `rows` that
    /// has cards.
    fn card_in(&self, mut rows: impl Iterator<Item = usize>, col: usize) -> Option<Spot> {
        rows.find(|&r| self.rows.get(r).is_some_and(|&n| n > 0))
            .map(|row| Spot::Card {
                row,
                col: col.min(self.rows[row] - 1),
            })
    }

    /// The last place above the rows of covers.
    fn above_cards(&self) -> Move {
        if self.songs > 0 {
            Move::To(Spot::Song(self.songs - 1))
        } else if self.top {
            Move::To(Spot::Top)
        } else {
            Move::Field
        }
    }

    /// Where `dir` takes the selection from `from`; with nothing selected, or a place the
    /// view no longer has, Down selects the first place.
    pub fn step(&self, from: Option<Spot>, dir: Dir) -> Move {
        let Some(from) = from.filter(|&s| self.holds(s)) else {
            return match (dir, self.first()) {
                (Dir::Down, Some(first)) => Move::To(first),
                _ => Move::Stay,
            };
        };
        let to = match (from, dir) {
            (Spot::Top, Dir::Up) => return Move::Field,
            (Spot::Top, Dir::Down) if self.songs > 0 => Some(Spot::Song(0)),
            (Spot::Top, Dir::Down) => self.card_in(0..self.rows.len(), 0),
            (Spot::Top, Dir::Right) => (self.songs > 0).then_some(Spot::Song(0)),
            (Spot::Top, Dir::Left) | (Spot::Song(_), Dir::Right) => None,
            (Spot::Song(0), Dir::Up) if self.top => Some(Spot::Top),
            (Spot::Song(0), Dir::Up) => return Move::Field,
            (Spot::Song(i), Dir::Up) => Some(Spot::Song(i - 1)),
            (Spot::Song(i), Dir::Down) if i + 1 < self.songs => Some(Spot::Song(i + 1)),
            (Spot::Song(_), Dir::Down) => self.card_in(0..self.rows.len(), 0),
            (Spot::Song(_), Dir::Left) => self.top.then_some(Spot::Top),
            (Spot::Card { row, col }, Dir::Left) => {
                col.checked_sub(1).map(|col| Spot::Card { row, col })
            }
            (Spot::Card { row, col }, Dir::Right) => {
                Some(Spot::Card { row, col: col + 1 }).filter(|&s| self.holds(s))
            }
            (Spot::Card { row, col }, Dir::Down) => self.card_in(row + 1..self.rows.len(), col),
            (Spot::Card { row, col }, Dir::Up) => match self.card_in((0..row).rev(), col) {
                Some(s) => Some(s),
                None => return self.above_cards(),
            },
        };
        to.map_or(Move::Stay, Move::To)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Dir::*;

    fn card(row: usize, col: usize) -> Move {
        Move::To(Spot::Card { row, col })
    }

    /// Follows `keys` from `from`, failing at a key that leaves the view or stays put.
    fn walk(shape: &Shape, from: Option<Spot>, keys: &[Dir]) -> Vec<Spot> {
        let mut at = from;
        keys.iter()
            .map(|&k| match shape.step(at, k) {
                Move::To(s) => {
                    at = Some(s);
                    s
                }
                m => panic!("{k:?} from {at:?} gave {m:?}"),
            })
            .collect()
    }

    #[test]
    fn down_goes_in_reading_order_from_the_top_result_to_the_last_row() {
        let shape = Shape {
            top: true,
            songs: 2,
            rows: vec![3, 2, 1],
        };
        let at = walk(&shape, None, &[Down, Down, Down, Down, Down, Down]);
        assert_eq!(
            at,
            [
                Spot::Top,
                Spot::Song(0),
                Spot::Song(1),
                Spot::Card { row: 0, col: 0 },
                Spot::Card { row: 1, col: 0 },
                Spot::Card { row: 2, col: 0 },
            ]
        );
        // The last row is the end.
        assert_eq!(shape.step(at.last().copied(), Down), Move::Stay);
        // And back up the same way, then out to the field.
        let up = walk(&shape, at.last().copied(), &[Up, Up, Up, Up, Up]);
        assert_eq!(up[2], Spot::Song(1));
        assert_eq!(up[4], Spot::Top);
        assert_eq!(shape.step(Some(Spot::Top), Up), Move::Field);
    }

    #[test]
    fn left_and_right_move_along_a_row_and_between_the_card_and_the_songs() {
        let shape = Shape {
            top: true,
            songs: 4,
            rows: vec![3],
        };
        assert_eq!(shape.step(Some(Spot::Top), Right), Move::To(Spot::Song(0)));
        assert_eq!(shape.step(Some(Spot::Song(2)), Left), Move::To(Spot::Top));
        assert_eq!(shape.step(Some(Spot::Song(2)), Right), Move::Stay);
        assert_eq!(shape.step(Some(Spot::Top), Left), Move::Stay);
        let first = Some(Spot::Card { row: 0, col: 0 });
        assert_eq!(shape.step(first, Right), card(0, 1));
        assert_eq!(shape.step(first, Left), Move::Stay);
        let last = Some(Spot::Card { row: 0, col: 2 });
        assert_eq!(shape.step(last, Right), Move::Stay);
        assert_eq!(shape.step(last, Left), card(0, 1));
        // Up from the first row of covers is the last song, from any column.
        assert_eq!(shape.step(last, Up), Move::To(Spot::Song(3)));
    }

    #[test]
    fn up_and_down_keep_the_column_or_take_a_shorter_rows_last_card() {
        let shape = Shape {
            top: true,
            songs: 1,
            rows: vec![6, 2, 5],
        };
        let far = Some(Spot::Card { row: 0, col: 4 });
        assert_eq!(shape.step(far, Down), card(1, 1));
        assert_eq!(
            shape.step(Some(Spot::Card { row: 2, col: 4 }), Up),
            card(1, 1)
        );
        assert_eq!(
            shape.step(Some(Spot::Card { row: 1, col: 1 }), Down),
            card(2, 1)
        );
        assert_eq!(
            shape.step(Some(Spot::Card { row: 2, col: 3 }), Down),
            Move::Stay
        );
    }

    #[test]
    fn empty_and_absent_sections_are_skipped() {
        // No songs: the card leads straight to the covers, and they back to it.
        let shape = Shape {
            top: true,
            songs: 0,
            rows: vec![0, 3],
        };
        assert_eq!(shape.step(Some(Spot::Top), Down), card(1, 0));
        assert_eq!(shape.step(Some(Spot::Top), Right), Move::Stay);
        assert_eq!(
            shape.step(Some(Spot::Card { row: 1, col: 2 }), Up),
            Move::To(Spot::Top)
        );
        // An empty row between two is passed over both ways.
        let shape = Shape {
            top: true,
            songs: 2,
            rows: vec![4, 0, 2],
        };
        assert_eq!(
            shape.step(Some(Spot::Card { row: 0, col: 3 }), Down),
            card(2, 1)
        );
        assert_eq!(
            shape.step(Some(Spot::Card { row: 2, col: 1 }), Up),
            card(0, 1)
        );
        // No top result: the first song is first, and up from it is the field.
        let shape = Shape {
            top: false,
            songs: 2,
            rows: vec![2],
        };
        assert_eq!(shape.step(None, Down), Move::To(Spot::Song(0)));
        assert_eq!(shape.step(Some(Spot::Song(0)), Up), Move::Field);
        assert_eq!(shape.step(Some(Spot::Song(0)), Left), Move::Stay);
        // Covers only.
        let shape = Shape {
            top: false,
            songs: 0,
            rows: vec![0, 2],
        };
        assert_eq!(shape.step(None, Down), card(1, 0));
        assert_eq!(
            shape.step(Some(Spot::Card { row: 1, col: 1 }), Up),
            Move::Field
        );
        // Nothing at all.
        assert_eq!(Shape::default().step(None, Down), Move::Stay);
    }

    #[test]
    fn nothing_selected_takes_only_down_and_a_lost_place_starts_over() {
        let shape = Shape {
            top: true,
            songs: 2,
            rows: vec![2],
        };
        for k in [Up, Left, Right] {
            assert_eq!(shape.step(None, k), Move::Stay);
        }
        assert_eq!(shape.step(None, Down), Move::To(Spot::Top));
        // A place the view no longer has, after the results changed.
        assert_eq!(shape.step(Some(Spot::Song(5)), Down), Move::To(Spot::Top));
        assert_eq!(
            shape.step(Some(Spot::Card { row: 3, col: 0 }), Up),
            Move::Stay
        );
    }
}
