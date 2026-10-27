use std::io::{self, BufRead, Write};
use win_can_utils::drivers::gs_usb::GsUsbAdapter;

pub fn print_adapters(adapters: &[GsUsbAdapter], output: &mut impl Write) -> io::Result<()> {
    if adapters.is_empty() {
        writeln!(output, "No gs_usb adapters are connected.")?;
        return Ok(());
    }
    writeln!(output, "Connected gs_usb adapters:")?;
    for (index, adapter) in adapters.iter().enumerate() {
        let serial = adapter
            .serial
            .as_deref()
            .map(|s| format!(" (serial {s})"))
            .unwrap_or_default();
        let status = adapter.unavailable_reason.as_deref().unwrap_or("Available");
        writeln!(
            output,
            "  {}) {}  {}{}  [{}]",
            index + 1,
            adapter.id,
            adapter.label(),
            serial,
            status
        )?;
    }
    Ok(())
}

pub fn select_adapter(
    adapters: &[GsUsbAdapter],
    interactive: bool,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> io::Result<String> {
    if adapters.len() == 1 && adapters[0].is_available() {
        return Ok(adapters[0].id.clone());
    }
    print_adapters(adapters, output)?;
    if !adapters.iter().any(GsUsbAdapter::is_available) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "No gs_usb adapters are available.",
        ));
    }
    if !interactive {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Multiple adapters are connected. Specify --device <ID> when running without an interactive terminal.",
        ));
    }
    loop {
        write!(
            output,
            "Select an available adapter [1-{}], or q to cancel: ",
            adapters.len()
        )?;
        output.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "No selection received. Specify --device <ID> for noninteractive use.",
            ));
        }
        let selection = line.trim();
        if selection.eq_ignore_ascii_case("q") {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Adapter selection cancelled.",
            ));
        }
        let adapter = selection
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|index| adapters.get(index));
        match adapter {
            Some(adapter) if adapter.is_available() => return Ok(adapter.id.clone()),
            Some(adapter) => writeln!(
                output,
                "{} is unavailable: {}",
                adapter.id,
                adapter
                    .unavailable_reason
                    .as_deref()
                    .unwrap_or("Unknown reason")
            )?,
            None => writeln!(output, "Enter a number from 1 to {}.", adapters.len())?,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapters() -> Vec<GsUsbAdapter> {
        vec![
            GsUsbAdapter {
                id: "001:001/0".into(),
                interface: Some(0),
                product: None,
                serial: None,
                unavailable_reason: Some("In use by another server".into()),
            },
            GsUsbAdapter {
                id: "001:002/0".into(),
                interface: Some(0),
                product: Some("CANable".into()),
                serial: Some("example".into()),
                unavailable_reason: None,
            },
        ]
    }

    #[test]
    fn busy_adapters_are_listed_and_cannot_be_selected() {
        let mut output = Vec::new();
        let selected =
            select_adapter(&adapters(), true, &mut &b"1\n0\n3\n2\n"[..], &mut output).unwrap();
        assert_eq!(selected, "001:002/0");
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("001:001/0") && text.contains("In use by another server"));
        assert!(text.contains("001:002/0") && text.contains("Available"));
        assert!(text.contains("is unavailable"));
    }

    #[test]
    fn multiple_adapters_require_explicit_selection_without_a_terminal() {
        let result = select_adapter(&adapters(), false, &mut &b""[..], &mut Vec::new());
        assert!(matches!(result, Err(e) if e.to_string().contains("--device")));
    }

    #[test]
    fn all_busy_and_empty_lists_do_not_prompt() {
        let busy = adapters().remove(0);
        for list in [vec![], vec![busy]] {
            let mut output = Vec::new();
            let result = select_adapter(&list, true, &mut &b""[..], &mut output);
            assert!(matches!(result, Err(e) if e.kind() == io::ErrorKind::NotFound));
            assert!(
                !String::from_utf8(output)
                    .unwrap()
                    .contains("Select an available")
            );
        }
    }

    #[test]
    fn single_available_adapter_opens_without_prompting() {
        let available = adapters().remove(1);
        let selected = select_adapter(&[available], false, &mut &b""[..], &mut Vec::new()).unwrap();
        assert_eq!(selected, "001:002/0");
    }
}
