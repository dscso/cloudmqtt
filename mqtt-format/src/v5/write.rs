//
//   This Source Code Form is subject to the terms of the Mozilla Public
//   License, v. 2.0. If a copy of the MPL was not distributed with this
//   file, You can obtain one at http://mozilla.org/MPL/2.0/.
//

pub type WResult<W> = core::result::Result<(), <W as WriteMqttPacket>::Error>;

#[derive(Debug)]
pub enum MqttWriteError {
    Invariant,
}

pub trait WriteMqttPacket: Send {
    type Error: From<MqttWriteError>;

    fn write_byte(&mut self, u: u8) -> impl core::future::Future<Output = WResult<Self>> + Send;
    fn write_slice(&mut self, u: &[u8])
        -> impl core::future::Future<Output = WResult<Self>> + Send;

    #[inline]
    fn write_u16(&mut self, u: u16) -> impl core::future::Future<Output = WResult<Self>> + Send {
        async move {
            self.write_byte((u >> 8) as u8).await?;
            self.write_byte(u as u8).await
        }
    }

    #[inline]
    fn write_u32(&mut self, u: u32) -> impl core::future::Future<Output = WResult<Self>> + Send {
        async move {
            let bytes = u.to_be_bytes();
            self.write_byte(bytes[0]).await?;
            self.write_byte(bytes[1]).await?;
            self.write_byte(bytes[2]).await?;
            self.write_byte(bytes[3]).await
        }
    }
}

#[cfg(test)]
mod test {
    use crate::v5::test::TestWriter;

    use super::WriteMqttPacket;

    #[tokio::test]
    async fn test_write_u32_1() {
        let mut writer = TestWriter { buffer: Vec::new() };
        writer.write_u32(1).await.unwrap();
        assert_eq!(writer.buffer, [0x00, 0x00, 0x00, 0x01]);
    }

    #[tokio::test]
    async fn test_write_u32_12() {
        let mut writer = TestWriter { buffer: Vec::new() };
        writer.write_u32(12).await.unwrap();
        assert_eq!(writer.buffer, [0x00, 0x00, 0x00, 12u8.to_be()]);
    }

    #[tokio::test]
    async fn test_write_u32_range() {
        // step by some prime number
        for i in (0..268_435_455).step_by(271) {
            let mut writer = TestWriter { buffer: Vec::new() };
            writer.write_u32(i).await.unwrap();
            assert_eq!(writer.buffer, i.to_be_bytes());
        }
    }
}