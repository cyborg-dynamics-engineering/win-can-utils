use crosscan::{can::CanFrame, protocol::*};
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn framing_handles_fragmentation_coalescing_and_binary_payload_values() {
    let (mut writer, mut reader) = tokio::io::duplex(1);
    let task = tokio::spawn(async move {
        for id in 0..16 {
            write_message(
                &mut writer,
                &Request {
                    version: VERSION,
                    id,
                    command: Command::Send {
                        frame: Frame::from(&CanFrame::new(1, &[0, 10, 255]).unwrap()),
                    },
                },
            )
            .await
            .unwrap();
        }
    });
    for id in 0..16 {
        let request: Request = read_message(&mut reader).await.unwrap();
        assert_eq!(request.id, id);
        assert!(matches!(request.command, Command::Send { frame } if frame.data == [0, 10, 255]));
    }
    task.await.unwrap();
}

#[tokio::test]
async fn oversized_and_truncated_messages_are_rejected() {
    for length in [0u32, (MAX_MESSAGE + 1) as u32] {
        let (mut writer, mut reader) = tokio::io::duplex(16);
        writer.write_u32_le(length).await.unwrap();
        assert!(read_message::<_, Request>(&mut reader).await.is_err());
    }
    let (mut writer, mut reader) = tokio::io::duplex(16);
    writer.write_u32_le(10).await.unwrap();
    writer.write_all(b"{}").await.unwrap();
    drop(writer);
    assert!(read_message::<_, Request>(&mut reader).await.is_err());
}

#[test]
fn filters_match_raw_flags_join_inversion_and_error_masks() {
    let standard = Frame::from(&CanFrame::new(0x123, &[]).unwrap());
    let extended = Frame::from(&CanFrame::new_eff(0x123, &[]).unwrap());
    let error = Frame::from(&CanFrame::new_error(0x40).unwrap());
    let mut subscription = Subscription {
        filters: vec![Filter {
            id: 0x123,
            mask: 0x8000_07ff,
            invert: false,
        }],
        ..Default::default()
    };
    assert!(subscription.matches(&standard));
    assert!(!subscription.matches(&extended));
    assert!(!subscription.matches(&error));
    subscription.error_mask = 0x40;
    assert!(subscription.matches(&error));
    subscription.filters.push(Filter {
        id: 0x123,
        mask: 0x7ff,
        invert: true,
    });
    subscription.join_filters = true;
    assert!(!subscription.matches(&standard));
    subscription.join_filters = false;
    assert!(subscription.matches(&standard));
    subscription.filters.clear();
    assert!(subscription.matches(&standard));
    assert!(subscription.matches(&extended));
}

#[test]
fn malformed_frames_and_configuration_limits_are_rejected() {
    for value in [
        serde_json::json!({"id":2048,"dlc":0}),
        serde_json::json!({"id":1,"dlc":9,"data":[0,0,0,0,0,0,0,0,0]}),
        serde_json::json!({"id":1,"dlc":2,"data":[0]}),
        serde_json::json!({"id":1,"dlc":1,"data":[0],"rtr":true}),
        serde_json::json!({"id":1,"dlc":1,"data":[0],"error":true}),
    ] {
        assert!(
            serde_json::from_value::<Frame>(value)
                .unwrap()
                .to_can()
                .is_err()
        );
    }
    for name in ["", "..", "can/0", "can\\0", "can0\0", "can 0"] {
        assert!(validate_name(name).is_err());
    }
    for capacity in [0, MAX_QUEUE + 1] {
        assert!(
            Subscription {
                queue_capacity: capacity,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }
}
