// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Serde helpers for object ids as 64 lowercase hex digits.

use braid_format::ObjectId;
use serde::{Deserialize, Deserializer, Serializer};

pub(crate) fn parse<E: serde::de::Error>(text: &str) -> Result<ObjectId, E> {
    text.parse::<ObjectId>()
        .map_err(|error| E::custom(format!("invalid object id {text:?}: {error}")))
}

/// One id.
pub(crate) mod id {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(id: &ObjectId, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&id.to_hex())
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<ObjectId, D::Error> {
        parse(&String::deserialize(d)?)
    }
}

/// An optional id, `null` for none.
pub(crate) mod opt_id {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(id: &Option<ObjectId>, s: S) -> Result<S::Ok, S::Error> {
        match id {
            Some(id) => s.serialize_str(&id.to_hex()),
            None => s.serialize_none(),
        }
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<ObjectId>, D::Error> {
        Option::<String>::deserialize(d)?
            .map(|text| parse(&text))
            .transpose()
    }
}

/// A list of ids.
pub(crate) mod ids {
    use super::*;
    use serde::ser::SerializeSeq;

    pub(crate) fn serialize<S: Serializer>(ids: &[ObjectId], s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(ids.len()))?;
        for id in ids {
            seq.serialize_element(&id.to_hex())?;
        }
        seq.end()
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<ObjectId>, D::Error> {
        Vec::<String>::deserialize(d)?
            .iter()
            .map(|text| parse::<D::Error>(text))
            .collect()
    }
}

/// A map from names to ids.
pub(crate) mod id_map {
    use super::*;
    use serde::ser::SerializeMap;
    use std::collections::BTreeMap;

    pub(crate) fn serialize<S: Serializer>(
        map: &BTreeMap<String, ObjectId>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        let mut out = s.serialize_map(Some(map.len()))?;
        for (name, id) in map {
            out.serialize_entry(name, &id.to_hex())?;
        }
        out.end()
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<BTreeMap<String, ObjectId>, D::Error> {
        BTreeMap::<String, String>::deserialize(d)?
            .into_iter()
            .map(|(name, text)| Ok((name, parse(&text)?)))
            .collect()
    }
}
