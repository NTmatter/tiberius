use asynchronous_codec::BytesMut;
use futures_util::io::{AsyncRead, AsyncWrite};
use tracing::{event, Level};

use crate::{
    client::Connection, sql_read_bytes::SqlReadBytes, BytesMutWithDataColumns, ExecuteResult, Row,
};

use super::{
    Encode, MetaDataColumn, PacketHeader, PacketStatus, TokenColMetaData, TokenDone, TokenRow,
    HEADER_BYTES,
};

/// A handler for a bulk insert data flow.
#[derive(Debug)]
pub struct BulkLoadRequest<'a, S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    connection: &'a mut Connection<S>,
    packet_id: u8,
    buf: BytesMut,
    columns: Vec<MetaDataColumn<'a>>,
}

impl<'a, S> BulkLoadRequest<'a, S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    pub(crate) fn new(
        connection: &'a mut Connection<S>,
        columns: Vec<MetaDataColumn<'a>>,
    ) -> crate::Result<Self> {
        let packet_id = connection.context_mut().next_packet_id();
        let mut buf = BytesMut::new();

        let cmd = TokenColMetaData {
            columns: columns.clone(),
        };

        cmd.encode(&mut buf)?;

        let this = Self {
            connection,
            packet_id,
            buf,
            columns,
        };

        Ok(this)
    }

    /// Adds a new row to the bulk insert, flushing only when having a full packet of data.
    ///
    /// # Warning
    ///
    /// After the last row, [`finalize`] must be called to flush the buffered
    /// data and for the data to actually be available in the table.
    ///
    /// [`finalize`]: #method.finalize
    pub async fn send(&mut self, row: TokenRow<'a>) -> crate::Result<()> {
        let mut buf_with_columns = BytesMutWithDataColumns::new(&mut self.buf, &self.columns);

        row.encode(&mut buf_with_columns)?;
        self.write_packets().await?;

        Ok(())
    }

    /// Adds a row to the bulk insert, ensuring that column types and names match
    /// before building and enqueuing each row.
    ///
    /// This function wraps `send`, organizing the input row to match the
    /// order of the destination table.
    ///
    /// # Warning
    ///
    /// After the last row, [`finalize`] must be called to flush the buffered
    /// data and for the data to actually be available in the table.
    ///
    /// [`finalize`]: #method.finalize
    pub async fn send_row(&mut self, row: Row) -> crate::Result<()> {
        // Ensure that column counts match.
        if &self.columns.len() != &row.len() {
            return Err(crate::Error::BulkInput(
                format!(
                    "Expecting {} columns but {} were given",
                    &self.columns.len(),
                    &row.len(),
                )
                .into(),
            ));
        }

        // Ensure that all required columns are present and correctly-ordered.
        let mut token_row = TokenRow::with_capacity(self.columns.len());
        for table_column in &self.columns {
            let column_name = &table_column.col_name;
            let Some(idx) = row.columns.iter().position(|col| col.name.eq(column_name)) else {
                return Err(crate::Error::BulkInput(
                    format!("Target has a column named {column_name} but it is not present in the input row")
                        .into(),
                ));
            };

            let Some(column_data) = row.data.get(idx) else {
                return Err(crate::Error::BulkInput(
                    format!("Input row has column {column_name} at {idx}, but row data does not have corresponding index")
                        .into(),
                ));
            };

            token_row.push(column_data.clone());
        }

        self.send(token_row).await
    }

    /// Ends the bulk load, flushing all pending data to the wire.
    ///
    /// This method must be called after sending all the data to flush all
    /// pending data and to get the server actually to store the rows to the
    /// table.
    pub async fn finalize(mut self) -> crate::Result<ExecuteResult> {
        TokenDone::default().encode(&mut self.buf)?;
        self.write_packets().await?;

        let mut header = PacketHeader::bulk_load(self.packet_id);
        header.set_status(PacketStatus::EndOfMessage);

        let data = self.buf.split();

        event!(
            Level::TRACE,
            "Finalizing a bulk insert ({} bytes)",
            data.len() + HEADER_BYTES,
        );

        self.connection.write_to_wire(header, data).await?;
        self.connection.flush_sink().await?;

        ExecuteResult::new(self.connection).await
    }

    async fn write_packets(&mut self) -> crate::Result<()> {
        let packet_size = (self.connection.context().packet_size() as usize) - HEADER_BYTES;

        while self.buf.len() > packet_size {
            let header = PacketHeader::bulk_load(self.packet_id);
            let data = self.buf.split_to(packet_size);

            event!(
                Level::TRACE,
                "Bulk insert packet ({} bytes)",
                data.len() + HEADER_BYTES,
            );

            self.connection.write_to_wire(header, data).await?;
        }

        Ok(())
    }
}
