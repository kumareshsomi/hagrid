use std::io::Cursor;
use std::convert::TryFrom;

use time;
use sequoia_openpgp::{packet::Signature, TPK, packet::UserID, Packet, PacketPile, constants::SignatureType, parse::Parse};
use Result;
use types::{Fingerprint, Email};

mod fs;
pub use self::fs::Filesystem;
mod memory;
pub use self::memory::Memory;
mod poly;
pub use self::poly::Polymorphic;

#[cfg(test)]
mod test;

#[derive(Serialize,Deserialize,Clone,Debug)]
pub struct Verify {
    created: i64,
    packets: Box<[u8]>,
    fpr: Fingerprint,
    email: Email,
}

impl Verify {
    pub fn new(uid: &UserID, sig: &[&Signature], fpr: Fingerprint) -> Result<Self> {
        use sequoia_openpgp::serialize::Serialize;

        let mut cur = Cursor::new(Vec::default());
        let res: Result<()> = uid.serialize(&mut cur)
            .map_err(|e| format!("sequoia_openpgp: {}", e).into());
        res?;

        for s in sig {
            let res: Result<()> = s.serialize(&mut cur)
                .map_err(|e| format!("sequoia_openpgp: {}", e).into());
            res?;
        }

        Ok(Verify{
            created: time::now().to_timespec().sec,
            packets: cur.into_inner().into(),
            fpr: fpr,
            email: Email::try_from(uid.clone())?,
        })
    }
}

#[derive(Serialize,Deserialize,Clone,Debug)]
pub struct Delete {
    created: i64,
    fpr: Fingerprint
}

impl Delete {
    pub fn new(fpr: Fingerprint) -> Self {
        Delete{
            created: time::now().to_timespec().sec,
            fpr: fpr
        }
    }
}

// uid -> uidsig+
// subkey -> subkeysig+

pub trait Database: Sync + Send {
    fn new_verify_token(&self, payload: Verify) -> Result<String>;
    fn new_delete_token(&self, payload: Delete) -> Result<String>;

    fn compare_and_swap(&self, fpr: &Fingerprint, present: Option<&[u8]>, new: Option<&[u8]>) -> Result<bool>;

    fn link_email(&self, email: &Email, fpr: &Fingerprint);
    fn unlink_email(&self, email: &Email, fpr: &Fingerprint);

    // (verified uid, fpr)
    fn pop_verify_token(&self, token: &str) -> Option<Verify>;
    // fpr
    fn pop_delete_token(&self, token: &str) -> Option<Delete>;

    fn by_fpr(&self, fpr: &Fingerprint) -> Option<Box<[u8]>>;
    fn by_email(&self, email: &Email) -> Option<Box<[u8]>>;
    // fn by_kid<'a>(&self, fpr: &str) -> Option<&[u8]>;

    fn strip_userids(tpk: TPK) -> Result<TPK> {
        let pile = tpk.to_packet_pile().into_children().filter(|pkt| {
            match pkt {
                &Packet::PublicKey(_) | &Packet::PublicSubkey(_) => true,
                &Packet::Signature(ref sig) =>
                    sig.sigtype() == SignatureType::DirectKey
                    || sig.sigtype() == SignatureType::SubkeyBinding
                    || sig.sigtype() == SignatureType::PrimaryKeyBinding,
                _ => false,
            }
        }).collect::<Vec<_>>();

        TPK::from_packet_pile(PacketPile::from_packets(pile))
            .map_err(|e| format!("sequoia_openpgp: {}", e).into())
    }

    fn tpk_into_bytes(tpk: &TPK) -> Result<Vec<u8>> {
        use std::io::Cursor;
        use sequoia_openpgp::serialize::Serialize;

        let mut cur = Cursor::new(Vec::default());
        tpk.serialize(&mut cur).map(|_| cur.into_inner()).map_err(|e| format!("{}", e).into())
    }

    fn merge_or_publish(&self, mut tpk: TPK) -> Result<Vec<(Email, String)>> {
        let fpr = Fingerprint::try_from(tpk.primary().fingerprint())?;
        let mut ret = Vec::default();

        // update verify tokens
        for uid in tpk.userids() {
            let email = Email::try_from(uid.userid().clone())?;

            if self.by_email(&email).is_none() {
                let payload = Verify::new(
                    uid.userid(),
                    &uid.selfsigs().collect::<Vec<_>>(),
                    fpr.clone())?;

                // XXX: send mail
                ret.push((email, self.new_verify_token(payload)?));
            }
        }

        tpk = Self::strip_userids(tpk)?;

        for _ in 0..100 /* while cas failed */ {
            // merge or update key db
            match self.by_fpr(&fpr).map(|x| x.to_vec()) {
                Some(old) => {
                    let new = TPK::from_bytes(&old).unwrap();
                    let new = new.merge(tpk.clone()).unwrap();
                    let new = Self::tpk_into_bytes(&new)?;

                    if self.compare_and_swap(&fpr, Some(&old), Some(&new))? {
                        return Ok(ret);
                    }
                }

                None => {
                    let fresh = Self::tpk_into_bytes(&tpk)?;

                    if self.compare_and_swap(&fpr, None, Some(&fresh))? {
                        return Ok(ret);
                    }
                }
            }
        }

        error!("Compare-and-swap of {} failed {} times in a row. Aborting.", fpr.to_string(), 100);
        Err("Database update failed".into())
    }

    // if (uid, fpr) = pop-token(tok) {
    //  while cas-failed() {
    //    tpk = by_fpr(fpr)
    //    merged = add-uid(tpk, uid)
    //    cas(tpk, merged)
    //  }
    // }
    fn verify_token(&self, token: &str) -> Result<Option<(Email, Fingerprint)>> {
        match self.pop_verify_token(token) {
            Some(Verify{ created, packets, fpr, email }) => {
                let now = time::now().to_timespec().sec;
                if created > now || now - created > 3 * 3600 { return Ok(None); }

                loop /* while cas falied */ {
                    match self.by_fpr(&fpr).map(|x| x.to_vec()) {
                        Some(old) => {
                            let mut new = old.clone();
                            new.extend(packets.into_iter());

                            if self.compare_and_swap(&fpr, Some(&old), Some(&new))? {
                                self.link_email(&email, &fpr);
                                return Ok(Some((email.clone(), fpr.clone())));
                            }
                        }
                        None => {
                            return Ok(None);
                        }
                    }
                }
            }
            None => Err("No such token".into()),
        }
    }

    fn request_deletion(&self, fpr: Fingerprint) -> Result<(String, Vec<Email>)> {
        match self.by_fpr(&fpr) {
            Some(tpk) => {
                let payload = Delete::new(fpr);
                let tok = self.new_delete_token(payload)?;
                let tpk = match TPK::from_bytes(&tpk) {
                    Ok(tpk) => tpk,
                    Err(e) => {
                        return Err(format!("Failed to parse TPK: {:?}", e).into());
                    }
                };
                let emails = tpk.userids().filter_map(|uid| {
                    Email::try_from(uid.userid().clone()).ok()
                }).collect::<Vec<_>>();

                Ok((tok, emails))
            }

            None => Err("Unknown key".into()),
        }
    }

    // if fpr = pop-token(tok) {
    //  tpk = by_fpr(fpr)
    //  for uid in tpk.userids {
    //    del-uid(uid)
    //  }
    //  del-fpr(fpr)
    // }
    fn confirm_deletion(&self, token: &str) -> Result<bool> {
        match self.pop_delete_token(token) {
            Some(Delete{ created, fpr }) => {
                let now = time::now().to_timespec().sec;
                if created > now || now - created > 3 * 3600 { return Ok(false); }

                loop {
                    match self.by_fpr(&fpr).map(|x| x.to_vec()) {
                        Some(old) => {
                            let tpk = match TPK::from_bytes(&old) {
                                Ok(tpk) => tpk,
                                Err(e) => {
                                    return Err(format!("Failed to parse old TPK: {:?}", e).into());
                                }
                            };

                            for uid in tpk.userids() {
                                self.unlink_email(&Email::try_from(uid.userid().clone())?, &fpr);
                            }

                            while !self.compare_and_swap(&fpr, Some(&old), None)? {}
                            return Ok(true);
                        }
                        None => {
                            return Ok(false);
                        }
                    }
                }
            }

            None => Ok(false),
        }
    }
}
