use uuid::{uuid, Uuid};

pub const SERVICE_UUID: Uuid = uuid!("cc7d466c-21f3-41ba-a711-991adf9f218e");
pub const TX_CHAR_UUID: Uuid = uuid!("9062d406-00b0-484a-ba3c-706fcf455e2f"); // Notify (Win→Mac)
pub const RX_CHAR_UUID: Uuid = uuid!("24bce5b3-4e33-426f-97be-f700c08714d8"); // WriteWithResponse (Mac→Win)

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_are_distinct() {
        assert_ne!(SERVICE_UUID, TX_CHAR_UUID);
        assert_ne!(SERVICE_UUID, RX_CHAR_UUID);
        assert_ne!(TX_CHAR_UUID, RX_CHAR_UUID);
    }

    #[test]
    fn uuids_are_v4() {
        for u in [SERVICE_UUID, TX_CHAR_UUID, RX_CHAR_UUID] {
            assert_eq!(u.get_version_num(), 4, "{u} should be v4 random UUID");
        }
    }
}
