use crossterm::event::{self, Event as CEvent, KeyEvent, MouseEvent};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize,
    FocusGained,
    FocusLost,
    Paste(String),
}

pub async fn read() -> Result<Event, std::io::Error> {
    tokio::task::spawn_blocking(move || match event::poll(Duration::from_millis(50)) {
        Ok(true) => match event::read() {
            Ok(ev) => match ev {
                CEvent::Key(k) => Ok(Event::Key(k)),
                CEvent::Mouse(m) => Ok(Event::Mouse(m)),
                CEvent::Resize(_, _) => Ok(Event::Resize),
                CEvent::FocusGained => Ok(Event::FocusGained),
                CEvent::FocusLost => Ok(Event::FocusLost),
                CEvent::Paste(s) => Ok(Event::Paste(s)),
            },
            Err(e) => Err(e),
        },
        Ok(false) => Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "timeout",
        )),
        Err(e) => Err(e),
    })
    .await?
}
