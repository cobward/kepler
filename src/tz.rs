use crate::{
    auth::{Action, AuthorizationPolicy, AuthorizationToken},
    manifest::Manifest,
    resource::OrbitId,
};
use anyhow::Result;
use nom::{
    branch::alt,
    bytes::complete::{tag, take_until},
    multi::many1,
    sequence::{preceded, tuple},
    IResult,
};
use rocket::request::{FromRequest, Outcome, Request};
use ssi::{
    did::DIDURL,
    jws::verify_bytes,
    tzkey::{decode_tzsig, jwk_from_tezos_key},
};
use std::str::FromStr;

#[derive(Debug, Clone)]
pub struct TezosAuthorizationString {
    pub sig: String,
    pub domain: String,
    pub pk: String,
    pub pkh: String,
    pub timestamp: String,
    pub orbit: OrbitId,
    pub action: Action,
}

impl FromStr for TezosAuthorizationString {
    type Err = anyhow::Error;
    fn from_str<'a>(s: &'a str) -> Result<Self, Self::Err> {
        match tuple::<_, _, nom::error::Error<&'a str>, _>((
            tag("Tezos Signed Message:"), // remove
            space_delimit,                // domain string
            space_delimit,                // get timestamp
            space_delimit,                // get pk
            space_delimit,                // get pkh
            space_delimit,                // get orbit
            tag(" "),
            parse_action, // get action
            tag(" "),
        ))(s)
        {
            Ok((
                sig_str,
                (_, domain_str, timestamp_str, pk_str, pkh_str, orbit_str, _, action, _),
            )) => Ok(TezosAuthorizationString {
                sig: sig_str.into(),
                domain: domain_str.into(),
                pk: pk_str.into(),
                pkh: pkh_str.into(),
                timestamp: timestamp_str.into(),
                orbit: orbit_str.parse()?,
                action,
            }),
            // TODO there is a lifetime issue which prevents using the nom error here
            Err(_) => Err(anyhow!("TzAuth Parsing Failed")),
        }
    }
}

fn space_delimit(s: &str) -> IResult<&str, &str> {
    preceded(tag(" "), take_until(" "))(s)
}

fn parse_list(s: &str) -> IResult<&str, Action> {
    tag("LIST")(s).map(|(_, rest)| (rest, Action::List))
}

fn parse_get(s: &str) -> IResult<&str, Action> {
    tuple((tag("GET"), many1(space_delimit)))(s).map(|(rest, (_, content))| {
        (
            rest,
            Action::Get(content.iter().map(|s| String::from(*s)).collect()),
        )
    })
}

fn parse_put(s: &str) -> IResult<&str, Action> {
    tuple((tag("PUT"), many1(space_delimit)))(s).map(|(rest, (_, content))| {
        (
            rest,
            Action::Put(content.iter().map(|s| String::from(*s)).collect()),
        )
    })
}

fn parse_del(s: &str) -> IResult<&str, Action> {
    tuple((tag("DEL"), many1(space_delimit)))(s).map(|(rest, (_, content))| {
        (
            rest,
            Action::Del(content.iter().map(|s| String::from(*s)).collect()),
        )
    })
}

fn parse_create(s: &str) -> IResult<&str, Action> {
    tuple((tag("CREATE"), many1(space_delimit)))(s).map(|(rest, (_, content))| {
        (
            rest,
            Action::Create {
                content: content.iter().map(|s| String::from(*s)).collect(),
            },
        )
    })
}

fn parse_action(s: &str) -> IResult<&str, Action> {
    alt((parse_get, parse_put, parse_del, parse_create, parse_list))(s)
}

fn serialize_action(action: &Action) -> Result<String> {
    match action {
        Action::Put(content) => serialize_content_action("PUT", content),
        Action::Get(content) => serialize_content_action("GET", content),
        Action::Del(content) => serialize_content_action("DEL", content),
        Action::List => Ok("LIST".into()),
        Action::Create { content } => Ok(["CREATE", &content.join(" ")].join(" ")),
    }
}

fn serialize_content_action(action: &str, content: &[String]) -> Result<String> {
    Ok([action, &content.join(" ")].join(" "))
}

impl TezosAuthorizationString {
    pub fn serialize(&self) -> Result<String> {
        Ok(format!(
            "Tezos Signed Message: {} {} {} {} {} {}",
            &self.domain,
            &self.timestamp,
            &self.pk,
            &self.pkh,
            &self.orbit,
            serialize_action(&self.action)?
        ))
    }

    fn serialize_for_verification(&self) -> Result<Vec<u8>> {
        Ok(encode_string(&self.serialize()?))
    }

    fn verify(&self) -> Result<()> {
        let key = jwk_from_tezos_key(&self.pk)?;
        let (_, sig) = decode_tzsig(&self.sig)?;
        Ok(verify_bytes(
            key.algorithm
                .ok_or_else(|| anyhow!("Invalid Signature Scheme"))?,
            &self.serialize_for_verification()?,
            &key,
            &sig,
        )?)
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for TezosAuthorizationString {
    type Error = anyhow::Error;
    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        match request
            .headers()
            .get_one("Authorization")
            .map(Self::from_str)
        {
            Some(Ok(t)) => Outcome::Success(t),
            _ => Outcome::Forward(()),
        }
    }
}

impl AuthorizationToken for TezosAuthorizationString {
    fn action(&self) -> &Action {
        &self.action
    }
    fn target_orbit(&self) -> &OrbitId {
        &self.orbit
    }
}

fn encode_string(s: &str) -> Vec<u8> {
    hex::decode(
        format!(
            "0501{:08x}{}",
            &s.as_bytes().len(),
            &hex::encode(&s.as_bytes())
        )
        .into_bytes(),
    )
    .unwrap()
}

impl core::fmt::Display for TezosAuthorizationString {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "Tezos Signed Message: {} {} {} {} {} {} {}",
            &self.domain,
            &self.timestamp,
            &self.pk,
            &self.pkh,
            &self.orbit,
            serialize_action(&self.action).map_err(|_| core::fmt::Error)?,
            &self.sig
        )
    }
}

#[rocket::async_trait]
impl AuthorizationPolicy<TezosAuthorizationString> for Manifest {
    async fn authorize(&self, auth_token: &TezosAuthorizationString) -> Result<()> {
        let requester = DIDURL {
            did: format!("did:pkh:tz:{}", &auth_token.pkh),
            fragment: Some("TezosMethod2021".to_string()),
            ..Default::default()
        };

        if !self.invokers().contains(&requester) {
            Err(anyhow!("Requester not a controller of the orbit"))
        } else {
            auth_token.verify()
        }
    }
}

#[test]
async fn string_encoding() {
    assert_eq!(
        &encode_string("message"),
        &[0x05, 0x01, 0x00, 0x00, 0x00, 0x07, 0x6d, 0x65, 0x73, 0x73, 0x61, 0x67, 0x65]
    )
}

#[test]
async fn simple_parse() {
    let auth_str = "Tezos Signed Message: kepler.net 2021-01-14T15:16:04Z edpkurFSehqm2HhLP9sZ4ZRW5nLZgyWErW8wYxgEUPHCMCy6Hk1tbm tz1Y6SXe4J9DBVuGM3GnWC2jnmDkA6fBVyjg kepler:did:pkh:tz:tz1Y6SXe4J9DBVuGM3GnWC2jnmDkA6fBVyjg://default PUT uAYAEHiB0uGRNPXEMdA9L-lXR2MKIZzKlgW1z6Ug4fSv3LRSPfQ edsigtmZ5tgugBSKjBJgptkm523C9EtVWrBhLYtv9MTAE6qF6mii2mFapdQfcCMsVzRisgQ3Nx61qC9Ut3VigyEC1s19RLwgkog";
    let _: TezosAuthorizationString = auth_str.parse().unwrap();
}

#[test]
#[should_panic]
async fn simple_verify_fail() {
    let auth_str = "Tezos Signed Message: kepler.net 2021-01-14T15:15:04Z edpkurFSehqm2HhLP9sZ4ZRW5nLZgyWErW8wYxgEUPHCMCy6Hk1tbm tz1Y6SXe4J9DBVuGM3GnWC2jnmDkA6fBVyjg kepler:did:pkh:tz:tz1Y6SXe4J9DBVuGM3GnWC2jnmDkA6fBVyjg://default PUT uAYAEHiB0uGRNPXEMdA9L-lXR2MKIZzKlgW1z6Ug4fSv3LRSPfQ edsigtmZ5tgugBSKjBJgptkm523C9EtVWrBhLYtv9MTAE6qF6mii2mFapdQfcCMsVzRisgQ3Nx61qC9Ut3VigyEC1s19RLwgkog";
    let tza: TezosAuthorizationString = auth_str.parse().unwrap();
    tza.verify().unwrap();
}

#[test]
async fn simple_verify_succeed() {
    let auth_str = "Tezos Signed Message: kepler.net 2021-01-14T15:16:04Z edpkuKVBnFUDhXhkFuKy7ufEKRdJR3jTqsaS3Es864idKCvvuwzJwc tz1fRW28ZrgzkPwQPqcZP5oWhtcAicA63Zxc kepler:did:example://my-orbit PUT uAYAEHiB0uGRNPXEMdA9L-lXR2MKIZzKlgW1z6Ug4fSv3LRSPfQ edsigtbGTq1J9A4YJsc2yUUnWNH1RdWs3RkqvfCRLuCEhRv7Y5UDKYiMABFkWLrW5BMdSxWnAdwzoWpzs83JPLVurFpbVWqycSB";
    let tza: TezosAuthorizationString = auth_str.parse().unwrap();
    tza.verify().unwrap();
}

#[test]
async fn round_trip() {
    use didkit::DID_METHODS;
    use ssi::{
        did::Source,
        jwk::{Algorithm, Params, JWK},
    };

    let ts = "2021-01-14T15:16:04Z";
    let dummy_cid = "uAYAEHiB0uGRNPXEMdA9L-lXR2MKIZzKlgW1z6Ug4fSv3LRSPfQ";
    let dummy_orbit = "kepler:did:example://my-orbit";
    let j = JWK::generate_ed25519().unwrap();
    let did = DID_METHODS
        .generate(&Source::KeyAndPattern(&j, "tz"))
        .unwrap();
    let pkh = did.split(':').last().unwrap();
    let pk: String = match &j.params {
        Params::OKP(p) => bs58::encode(
            [13, 15, 37, 217]
                .iter()
                .chain(&p.public_key.0)
                .copied()
                .collect::<Vec<u8>>(),
        )
        .with_check()
        .into_string(),
        _ => panic!(),
    };
    let tz_unsigned = TezosAuthorizationString {
        sig: "".into(),
        domain: "kepler.net".into(),
        pk,
        pkh: pkh.into(),
        timestamp: ts.into(),
        orbit: dummy_orbit.parse().unwrap(),
        action: Action::Put(vec![dummy_cid.to_string()]),
    };
    let message = tz_unsigned
        .serialize_for_verification()
        .expect("failed to serialize authz message");
    let sig_bytes = ssi::jws::sign_bytes(Algorithm::EdBlake2b, &message, &j).unwrap();
    let sig = bs58::encode(
        [9, 245, 205, 134, 18]
            .iter()
            .chain(&sig_bytes)
            .copied()
            .collect::<Vec<u8>>(),
    )
    .with_check()
    .into_string();
    let tz = TezosAuthorizationString { sig, ..tz_unsigned };

    assert_eq!(
        message,
        tz.serialize_for_verification()
            .expect("failed to serialize authz message")
    );
    assert!(tz.verify().is_ok());
}
