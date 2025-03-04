use std::{collections::HashMap, rc::Rc, sync::Arc};

use log::warn;
use unicode_segmentation::UnicodeSegmentation;

use crate::{
    bridge::GridLineCell,
    editor::{grid::CharacterGrid, style::Style, AnchorInfo, DrawCommand, DrawCommandBatcher},
    renderer::{LineFragment, WindowDrawCommand},
};

pub enum WindowType {
    Editor,
    Message,
}

pub struct Window {
    grid_id: u64,
    grid: CharacterGrid,
    pub window_type: WindowType,

    pub anchor_info: Option<AnchorInfo>,
    grid_position: (f64, f64),

    draw_command_batcher: Rc<DrawCommandBatcher>,
}

impl Window {
    pub fn new(
        grid_id: u64,
        window_type: WindowType,
        anchor_info: Option<AnchorInfo>,
        grid_position: (f64, f64),
        grid_size: (u64, u64),
        draw_command_batcher: Rc<DrawCommandBatcher>,
    ) -> Window {
        let window = Window {
            grid_id,
            grid: CharacterGrid::new(grid_size),
            window_type,
            anchor_info,
            grid_position,
            draw_command_batcher,
        };
        window.send_updated_position();
        window
    }

    fn send_command(&self, command: WindowDrawCommand) {
        self.draw_command_batcher
            .queue(DrawCommand::Window {
                grid_id: self.grid_id,
                command,
            })
            .ok();
    }

    fn send_updated_position(&self) {
        self.send_command(WindowDrawCommand::Position {
            grid_position: self.grid_position,
            grid_size: (self.grid.width, self.grid.height),
            floating_order: self.anchor_info.clone().map(|anchor| anchor.sort_order),
        });
    }

    pub fn get_cursor_grid_cell(
        &self,
        window_left: u64,
        window_top: u64,
    ) -> (String, Option<Arc<Style>>, bool) {
        let grid_cell = match self.grid.get_cell(window_left, window_top) {
            Some((character, style)) => (character.clone(), style.clone()),
            _ => (' '.to_string(), None),
        };

        let double_width = match self.grid.get_cell(window_left + 1, window_top) {
            Some((character, _)) => character.is_empty(),
            _ => false,
        };

        (grid_cell.0, grid_cell.1, double_width)
    }

    pub fn get_width(&self) -> u64 {
        self.grid.width
    }

    pub fn get_height(&self) -> u64 {
        self.grid.height
    }

    pub fn get_grid_position(&self) -> (f64, f64) {
        self.grid_position
    }

    pub fn position(
        &mut self,
        anchor_info: Option<AnchorInfo>,
        grid_size: (u64, u64),
        grid_position: (f64, f64),
    ) {
        self.grid.resize(grid_size);
        self.anchor_info = anchor_info;
        self.grid_position = grid_position;
        self.send_updated_position();
        self.redraw();
    }

    pub fn resize(&mut self, new_size: (u64, u64)) {
        self.grid.resize(new_size);
        self.send_updated_position();
        self.redraw();
    }

    fn modify_grid(
        &mut self,
        row_index: u64,
        column_pos: &mut u64,
        cell: GridLineCell,
        defined_styles: &HashMap<u64, Arc<Style>>,
        previous_style: &mut Option<Arc<Style>>,
    ) {
        // Get the defined style from the style list.
        let style = match cell.highlight_id {
            Some(0) => None,
            Some(style_id) => defined_styles.get(&style_id).cloned(),
            None => previous_style.clone(),
        };

        // Compute text.
        let mut text = cell.text;
        if let Some(times) = cell.repeat {
            // Repeats of zero times should be ignored, they are mostly useful for terminal Neovim
            // to distinguish between empty lines and lines ending with spaces.
            if times == 0 {
                return;
            }
            text = text.repeat(times as usize);
        }

        // Insert the contents of the cell into the grid.
        if text.is_empty() {
            if let Some(cell) = self.grid.get_cell_mut(*column_pos, row_index) {
                *cell = (text, style.clone());
            }
            *column_pos += 1;
        } else {
            for character in text.graphemes(true) {
                if let Some(cell) = self.grid.get_cell_mut(*column_pos, row_index) {
                    *cell = (character.to_string(), style.clone());
                }
                *column_pos += 1;
            }
        }

        *previous_style = style;
    }

    // Build a line fragment for the given row starting from current_start up until the next style
    // change or double width character.
    fn build_line_fragment(&self, row_index: u64, start: u64) -> (u64, LineFragment) {
        let row = self.grid.row(row_index).unwrap();

        let (_, style) = &row[start as usize];

        let mut text = String::new();
        let mut width = 0;
        for possible_end_index in start..self.grid.width {
            let (character, possible_end_style) = &row[possible_end_index as usize];

            // Style doesn't match. Draw what we've got.
            if style != possible_end_style {
                break;
            }

            width += 1;
            // The previous character is double width, so send this as its own draw command.
            if character.is_empty() {
                break;
            }

            // Add the grid cell to the cells to render.
            text.push_str(character);
        }

        let line_fragment = LineFragment {
            text,
            window_left: start,
            window_top: row_index,
            width,
            style: style.clone(),
        };

        (start + width, line_fragment)
    }

    // Redraw line by calling build_line_fragment starting at 0
    // until current_start is greater than the grid width and sending the resulting
    // fragments as a batch.
    fn redraw_line(&self, row: u64) {
        let mut current_start = 0;
        let mut line_fragments = Vec::new();
        while current_start < self.grid.width {
            let (next_start, line_fragment) = self.build_line_fragment(row, current_start);
            current_start = next_start;
            line_fragments.push(line_fragment);
        }
        self.send_command(WindowDrawCommand::DrawLine(line_fragments));
    }

    pub fn draw_grid_line(
        &mut self,
        row: u64,
        column_start: u64,
        cells: Vec<GridLineCell>,
        defined_styles: &HashMap<u64, Arc<Style>>,
    ) {
        let mut previous_style = None;
        if row < self.grid.height {
            let mut column_pos = column_start;
            for cell in cells {
                self.modify_grid(
                    row,
                    &mut column_pos,
                    cell,
                    defined_styles,
                    &mut previous_style,
                );
            }

            // Due to the limitations of the current rendering strategy, some underlines get
            // clipped by the line below. To mitigate that, we redraw the adjacent lines whenever
            // an individual line is redrawn. Unfortunately, some clipping still happens.
            // TODO: figure out how to solve this
            if row < self.grid.height - 1 {
                self.redraw_line(row + 1);
            }
            self.redraw_line(row);
            if row > 0 {
                self.redraw_line(row - 1);
            }
        } else {
            warn!("Draw command out of bounds");
        }
    }

    pub fn scroll_region(
        &mut self,
        top: u64,
        bottom: u64,
        left: u64,
        right: u64,
        rows: i64,
        cols: i64,
    ) {
        let mut top_to_bottom;
        let mut bottom_to_top;
        let y_iter: &mut dyn Iterator<Item = i64> = if rows > 0 {
            top_to_bottom = (top as i64 + rows)..bottom as i64;
            &mut top_to_bottom
        } else {
            bottom_to_top = (top as i64..(bottom as i64 + rows)).rev();
            &mut bottom_to_top
        };

        self.send_command(WindowDrawCommand::Scroll {
            top,
            bottom,
            left,
            right,
            rows,
            cols,
        });

        // Scrolls must not only translate the rendered texture, but also must move the grid data
        // accordingly so that future renders work correctly.
        for y in y_iter {
            let dest_y = y - rows;
            let mut cols_left;
            let mut cols_right;
            if dest_y >= 0 && dest_y < self.grid.height as i64 {
                let x_iter: &mut dyn Iterator<Item = i64> = if cols > 0 {
                    cols_left = (left as i64 + cols)..right as i64;
                    &mut cols_left
                } else {
                    cols_right = (left as i64..(right as i64 + cols)).rev();
                    &mut cols_right
                };

                for x in x_iter {
                    let dest_x = x - cols;
                    let cell_data = self.grid.get_cell(x as u64, y as u64).cloned();

                    if let Some(cell_data) = cell_data {
                        if let Some(dest_cell) =
                            self.grid.get_cell_mut(dest_x as u64, dest_y as u64)
                        {
                            *dest_cell = cell_data;
                        }
                    }
                }
            }
        }
    }

    pub fn clear(&mut self) {
        self.grid.clear();
        self.send_command(WindowDrawCommand::Clear);
    }

    pub fn redraw(&self) {
        self.send_command(WindowDrawCommand::Clear);
        // Draw the lines from the bottom up so that underlines don't get overwritten by the line
        // below.
        for row in (0..self.grid.height).rev() {
            self.redraw_line(row);
        }
    }

    pub fn hide(&self) {
        self.send_command(WindowDrawCommand::Hide);
    }

    pub fn show(&self) {
        self.send_command(WindowDrawCommand::Show);
    }

    pub fn close(&self) {
        self.send_command(WindowDrawCommand::Close);
    }

    pub fn update_viewport(&self, scroll_delta: f64) {
        self.send_command(WindowDrawCommand::Viewport { scroll_delta });
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::event_aggregator::EVENT_AGGREGATOR;

    #[test]
    fn window_separator_modifies_grid_and_sends_draw_command() {
        let mut draw_command_receiver = EVENT_AGGREGATOR.register_event::<Vec<DrawCommand>>();
        let draw_command_batcher = Rc::new(DrawCommandBatcher::new());

        let mut window = Window::new(
            1,
            WindowType::Editor,
            None,
            (0.0, 0.0),
            (114, 64),
            draw_command_batcher.clone(),
        );

        draw_command_batcher.send_batch();

        draw_command_receiver
            .try_recv()
            .expect("Could not receive commands");

        window.draw_grid_line(
            1,
            70,
            vec![GridLineCell {
                text: "|".to_owned(),
                highlight_id: None,
                repeat: None,
            }],
            &HashMap::new(),
        );

        assert_eq!(window.grid.get_cell(70, 1), Some(&("|".to_owned(), None)));

        draw_command_batcher.send_batch();

        let sent_commands = draw_command_receiver
            .try_recv()
            .expect("Could not receive commands");
        assert!(!sent_commands.is_empty());
    }
}
