#[derive(Debug)]
pub(crate) struct Chord {
    pub(crate) action: crate::hotkey::Action,
    pub(crate) key: u16,
    pub(crate) modifiers: Option<u8>,
}
