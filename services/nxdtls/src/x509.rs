//! What the client needs of the server's leaf certificate, read from its
//! DER with every length checked: the subject's common name and the
//! SubjectPublicKeyInfo (an RSA key here). The chain is not verified: the
//! session API pins the leaf (SHA-256 of its DER and its CN).

/// One DER element: tag, content, and what follows it.
fn element(d: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = d.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (first as usize, rest)
    } else {
        let n = (first & 0x7F) as usize;
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        let len = rest[..n].iter().fold(0usize, |a, &b| a << 8 | b as usize);
        (len, &rest[n..])
    };
    (rest.len() >= len).then(|| (tag, &rest[..len], &rest[len..]))
}

/// A child element: tag, content, the whole element (header included).
type Child<'a> = (u8, &'a [u8], &'a [u8]);

/// The elements of a SEQUENCE or SET's content, in order.
fn children(mut d: &[u8]) -> Option<Vec<Child<'_>>> {
    let mut out = Vec::new();
    while !d.is_empty() {
        let (tag, content, rest) = element(d)?;
        // The whole element, header included, for SPKI.
        let whole = &d[..d.len() - rest.len()];
        out.push((tag, content, whole));
        d = rest;
    }
    Some(out)
}

const OID_CN: &[u8] = &[0x55, 0x04, 0x03];

pub struct Leaf<'a> {
    pub common_name: String,
    /// The DER of the SubjectPublicKeyInfo.
    pub spki: &'a [u8],
}

/// Certificate → TBSCertificate → (version?, serial, signature, issuer,
/// validity, subject, subjectPublicKeyInfo, ...).
pub fn leaf(der: &[u8]) -> Option<Leaf<'_>> {
    let (0x30, cert, _) = element(der)? else {
        return None;
    };
    let (0x30, tbs, _) = element(cert)? else {
        return None;
    };
    let mut f = children(tbs)?;
    if f.first()?.0 == 0xA0 {
        f.remove(0);
    }
    // serial, signature, issuer, validity, subject, spki
    let subject = f.get(4)?;
    let spki = f.get(5)?;
    if subject.0 != 0x30 || spki.0 != 0x30 {
        return None;
    }
    Some(Leaf {
        common_name: common_name(subject.1)?,
        spki: spki.2,
    })
}

/// The last CN of a Name (SEQUENCE of SET of AttributeTypeAndValue).
fn common_name(name: &[u8]) -> Option<String> {
    let mut cn = None;
    for (_, set, _) in children(name)? {
        for (_, atv, _) in children(set)? {
            let kv = children(atv)?;
            if kv.len() == 2 && kv[0].0 == 0x06 && kv[0].1 == OID_CN {
                cn = Some(String::from_utf8_lossy(kv[1].1).into_owned());
            }
        }
    }
    cn
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_and_long_lengths() {
        assert_eq!(
            element(&[0x04, 0x02, 1, 2, 9]),
            Some((4, &[1u8, 2][..], &[9u8][..]))
        );
        let long = [&[0x04, 0x81, 0x80][..], &[0u8; 0x80]].concat();
        assert_eq!(element(&long).unwrap().1.len(), 0x80);
        assert!(element(&[0x04, 0x05, 1]).is_none());
        assert!(element(&[0x04, 0x85, 1, 1, 1, 1, 1]).is_none());
    }

    #[test]
    fn garbage_is_no_certificate() {
        assert!(leaf(b"not a certificate").is_none());
        assert!(leaf(&[0x30, 0x00]).is_none());
    }
}
