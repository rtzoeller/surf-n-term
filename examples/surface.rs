use std::{boxed::Box, error::Error, time::Duration};
use tty_surface::{Face, Renderer, Surface, SystemTerminal, Terminal, View};

fn main() -> Result<(), Box<dyn Error>> {
    let bg = Face::default().with_bg(Some("#3c3836".parse()?));
    let one = Face::default().with_bg(Some("#d3869b".parse()?));
    let two = Face::default().with_bg(Some("#b8bb26".parse()?));
    let three = Face::default().with_bg(Some("#fb4934".parse()?));

    let mut surface = Surface::new(10, 20);
    surface.fill(|_, _, cell| cell.face = bg);
    surface.view(..2, ..2).fill(|_, _, cell| cell.face = one);
    surface.view(-2.., -2..).fill(|_, _, cell| cell.face = two);
    surface.view(.., 3..4).fill(|_, _, cell| cell.face = three);
    surface
        .view(3..4, ..-1)
        .fill(|_, _, cell| cell.face = three);

    let mut term = SystemTerminal::new()?;
    term.render(&surface)?;
    term.poll(Some(Duration::from_secs(0)))?;

    Ok(())
}