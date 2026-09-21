//! Typed access to the measured fixture files embedded at compile time.

pub enum FixtureKind {
    OauthNonFirstParty,
    OauthFirstParty,
    ApiKey,
    Hello,
    CountOauthNonFirstParty,
    CountOauthFirstParty,
    CountApiKey,
}
pub enum BodyFixture {
    FirstParty9b244,
    FirstPartyE142e,
    Oauth,
    ApiKey,
    CountOauth,
    CountOauthFirstParty,
    CountApiKey,
}
pub enum Plane {
    Inference,
    Control,
}
pub enum ControlFixture {
    Token,
    Profile,
    Roles,
    Usage,
}

pub fn inference_head(kind: FixtureKind) -> &'static [u8] {
    match kind {
        FixtureKind::OauthNonFirstParty => include_bytes!("../fixtures/inference_oauth.http"),
        FixtureKind::OauthFirstParty => include_bytes!("../fixtures/inference_oauth_fp.http"),
        FixtureKind::ApiKey => include_bytes!("../fixtures/inference_apikey.http"),
        FixtureKind::Hello => include_bytes!("../fixtures/hello.http"),
        FixtureKind::CountOauthNonFirstParty => include_bytes!("../fixtures/count_oauth.http"),
        FixtureKind::CountOauthFirstParty => include_bytes!("../fixtures/count_oauth_fp.http"),
        FixtureKind::CountApiKey => include_bytes!("../fixtures/count_apikey.http"),
    }
}

pub fn body(kind: BodyFixture) -> &'static [u8] {
    match kind {
        BodyFixture::FirstParty9b244 => include_bytes!("../fixtures/body_fp_9b244.json"),
        BodyFixture::FirstPartyE142e => include_bytes!("../fixtures/body_fp_e142e.json"),
        BodyFixture::Oauth => include_bytes!("../fixtures/body_oauth.json"),
        BodyFixture::ApiKey => include_bytes!("../fixtures/body_apikey.json"),
        BodyFixture::CountOauth => include_bytes!("../fixtures/count_oauth.json"),
        BodyFixture::CountOauthFirstParty => include_bytes!("../fixtures/count_oauth_fp.json"),
        BodyFixture::CountApiKey => include_bytes!("../fixtures/count_apikey.json"),
    }
}

pub fn client_hello(plane: Plane) -> Vec<u8> {
    let hex = match plane {
        Plane::Inference => include_str!("../fixtures/hello_inference.hex"),
        Plane::Control => include_str!("../fixtures/hello_control.hex"),
    };
    let hex = hex.trim();
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("fixture hex"))
        .collect()
}

pub fn control_head(which: ControlFixture) -> &'static [u8] {
    match which {
        ControlFixture::Token => include_bytes!("../fixtures/control_token.http"),
        ControlFixture::Profile => include_bytes!("../fixtures/control_profile.http"),
        ControlFixture::Roles => include_bytes!("../fixtures/control_roles.http"),
        ControlFixture::Usage => include_bytes!("../fixtures/control_usage.http"),
    }
}

pub fn control_response(which: ControlFixture) -> &'static [u8] {
    match which {
        ControlFixture::Token => include_bytes!("../fixtures/responses/token.json"),
        ControlFixture::Profile => include_bytes!("../fixtures/responses/profile.json"),
        ControlFixture::Roles => include_bytes!("../fixtures/responses/roles.json"),
        ControlFixture::Usage => include_bytes!("../fixtures/responses/usage.json"),
    }
}
