use bdk::bitcoin::Amount;
use bdk::bitcoin::OutPoint;
use bdk::bitcoin::Script;
use bdk::bitcoin::Transaction;
use maia_core::TransactionExt as _;

pub trait TransactionExt {
    fn find_output_amount(&self, script_pubkey: &Script) -> Option<Amount>;
}

impl TransactionExt for Transaction {
    fn find_output_amount(&self, script_pubkey: &Script) -> Option<Amount> {
        let OutPoint {
            vout: maker_vout, ..
        } = match self.outpoint(script_pubkey) {
            Ok(out_point) => out_point,
            Err(_) => return None,
        };

        Some(Amount::from_sat(self.output[maker_vout as usize].value))
    }
}
