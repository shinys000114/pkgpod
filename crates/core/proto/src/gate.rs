// SPDX-License-Identifier: AGPL-3.0-only

//! The rights rule: whether a request may go on, and if not, why and
//! with which status. One rule for every format, in a fixed order, so
//! that what a caller is told (401, 404, 405 or 403) never depends on
//! which format answers:
//!
//! 1. no role, anonymous: authentication required (401);
//! 2. no role, logged in: as if the repository were not there (404);
//! 3. the route needs a login and the caller is anonymous: 401;
//! 4. a write to a proxy or a group: they are read through (405);
//! 5. a write to a labelled version: labels are read only (405);
//! 6. a role below the route's, anonymous: 401 (clients send their
//!    credentials only after one);
//! 7. a role below the route's, logged in: 403.

use pkgpod_engine::Kind;

/// A caller's role on a repository (or a route's need), lowest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Read,
    Write,
    Admin,
}

/// What a route needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Need {
    pub right: Role,
    /// A logged-in caller, whatever the repository lets anyone do (who
    /// am I, a client's token).
    pub login: bool,
    /// The route changes the repository.
    pub writes: bool,
}

impl Need {
    pub const READ: Need = Need {
        right: Role::Read,
        login: false,
        writes: false,
    };
    pub const WRITE: Need = Need {
        right: Role::Write,
        login: false,
        writes: true,
    };
}

/// What the rule looks at.
#[derive(Debug, Clone, Copy)]
pub struct Gate {
    /// The caller's role on the repository (`None`: none, not even read).
    pub role: Option<Role>,
    pub anonymous: bool,
    pub need: Need,
    pub kind: Kind,
    /// The request is for a labelled version.
    pub labelled: bool,
}

/// Why a request does not go on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// 401, with the format's challenge.
    Unauthenticated,
    /// 404: a caller with no role is not told the repository is there.
    Hidden,
    /// 405: proxies and groups are read through.
    ReadThrough,
    /// 405: a labelled version is not written to.
    LabelReadOnly,
    /// 403.
    Forbidden,
}

impl Refusal {
    pub fn status(self) -> u16 {
        match self {
            Refusal::Unauthenticated => 401,
            Refusal::Hidden => 404,
            Refusal::ReadThrough | Refusal::LabelReadOnly => 405,
            Refusal::Forbidden => 403,
        }
    }
}

/// The rule, in its order.
pub fn decide(g: &Gate) -> Result<(), Refusal> {
    let Some(role) = g.role else {
        return Err(if g.anonymous {
            Refusal::Unauthenticated
        } else {
            Refusal::Hidden
        });
    };
    if g.need.login && g.anonymous {
        return Err(Refusal::Unauthenticated);
    }
    if g.need.writes && matches!(g.kind, Kind::Proxy | Kind::Group) {
        return Err(Refusal::ReadThrough);
    }
    if g.need.writes && g.labelled {
        return Err(Refusal::LabelReadOnly);
    }
    if role < g.need.right {
        return Err(if g.anonymous {
            Refusal::Unauthenticated
        } else {
            Refusal::Forbidden
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (case, role, anonymous, need, kind, labelled, today's status, what
    /// the rule gives, the row that changes it)
    type Case = (
        &'static str,
        Option<Role>,
        bool,
        Need,
        Kind,
        bool,
        u16,
        Result<(), Refusal>,
        Option<&'static str>,
    );

    /// The answers the request corpus records today (files, Debian, OCI,
    /// npm and cargo repositories, tools/replay), held to the rule: its
    /// status equal but for the rows the rule changes on purpose, each
    /// named; and which refusal it is, as a format words a proxy's 405
    /// and a label's apart.
    #[test]
    fn the_rule_gives_what_is_answered_today_but_for_the_named_rows() {
        use Kind::*;
        let r = Some(Role::Read);
        let w = Some(Role::Write);
        let a = Some(Role::Admin);
        let login = Need {
            login: true,
            ..Need::READ
        };
        let cases: &[Case] = &[
            (
                "private read, anonymous",
                None,
                true,
                Need::READ,
                Hosted,
                false,
                401,
                Err(Refusal::Unauthenticated),
                None,
            ),
            (
                "private read, no grant",
                None,
                false,
                Need::READ,
                Hosted,
                false,
                404,
                Err(Refusal::Hidden),
                None,
            ),
            (
                "private read, reader",
                r,
                false,
                Need::READ,
                Hosted,
                false,
                200,
                Ok(()),
                None,
            ),
            (
                "public read, anonymous",
                r,
                true,
                Need::READ,
                Hosted,
                false,
                200,
                Ok(()),
                None,
            ),
            (
                "public write, anonymous",
                r,
                true,
                Need::WRITE,
                Hosted,
                false,
                401,
                Err(Refusal::Unauthenticated),
                None,
            ),
            (
                "public write, no grant",
                r,
                false,
                Need::WRITE,
                Hosted,
                false,
                403,
                Err(Refusal::Forbidden),
                None,
            ),
            (
                "private write, reader",
                r,
                false,
                Need::WRITE,
                Hosted,
                false,
                403,
                Err(Refusal::Forbidden),
                None,
            ),
            (
                "private write, writer",
                w,
                false,
                Need::WRITE,
                Hosted,
                false,
                201,
                Ok(()),
                None,
            ),
            (
                "files proxy write, anonymous",
                r,
                true,
                Need::WRITE,
                Proxy,
                false,
                405,
                Err(Refusal::ReadThrough),
                None,
            ),
            (
                "files proxy write, writer",
                r,
                false,
                Need::WRITE,
                Proxy,
                false,
                405,
                Err(Refusal::ReadThrough),
                None,
            ),
            (
                "private group read, no grant",
                None,
                false,
                Need::READ,
                Group,
                false,
                404,
                Err(Refusal::Hidden),
                None,
            ),
            (
                "private group read, reader",
                r,
                false,
                Need::READ,
                Group,
                false,
                200,
                Ok(()),
                None,
            ),
            // OCI proxies and groups answer a write as any write before
            // the rule: 401 or 403 by the role (R6).
            (
                "oci proxy push, anonymous",
                r,
                true,
                Need::WRITE,
                Proxy,
                false,
                401,
                Err(Refusal::ReadThrough),
                Some("R6"),
            ),
            (
                "oci proxy push, writer",
                r,
                false,
                Need::WRITE,
                Proxy,
                false,
                403,
                Err(Refusal::ReadThrough),
                Some("R6"),
            ),
            (
                "oci proxy push, admin",
                a,
                false,
                Need::WRITE,
                Proxy,
                false,
                405,
                Err(Refusal::ReadThrough),
                None,
            ),
            (
                "oci group push, reader",
                r,
                false,
                Need::WRITE,
                Group,
                false,
                403,
                Err(Refusal::ReadThrough),
                Some("R6"),
            ),
            (
                "oci group push, admin",
                a,
                false,
                Need::WRITE,
                Group,
                false,
                405,
                Err(Refusal::ReadThrough),
                None,
            ),
            (
                "oci private group push, no grant",
                None,
                false,
                Need::WRITE,
                Group,
                false,
                404,
                Err(Refusal::Hidden),
                None,
            ),
            (
                "npm whoami, anonymous",
                r,
                true,
                login,
                Hosted,
                false,
                401,
                Err(Refusal::Unauthenticated),
                None,
            ),
            (
                "npm whoami, no grant",
                r,
                false,
                login,
                Hosted,
                false,
                200,
                Ok(()),
                None,
            ),
            (
                "npm public publish, no grant",
                r,
                false,
                Need::WRITE,
                Hosted,
                false,
                403,
                Err(Refusal::Forbidden),
                None,
            ),
            (
                "npm private publish, reader",
                r,
                false,
                Need::WRITE,
                Hosted,
                false,
                403,
                Err(Refusal::Forbidden),
                None,
            ),
            // cargo refuses a write to a label before it looks at the
            // role, as the rule does.
            (
                "cargo labelled publish, anonymous",
                r,
                true,
                Need::WRITE,
                Hosted,
                true,
                405,
                Err(Refusal::LabelReadOnly),
                None,
            ),
            (
                "cargo labelled publish, no grant",
                r,
                false,
                Need::WRITE,
                Hosted,
                true,
                405,
                Err(Refusal::LabelReadOnly),
                None,
            ),
            (
                "cargo labelled publish, writer",
                w,
                false,
                Need::WRITE,
                Hosted,
                true,
                405,
                Err(Refusal::LabelReadOnly),
                None,
            ),
            // A labelled version is read as the current one is.
            (
                "cargo labelled read, anonymous",
                r,
                true,
                Need::READ,
                Hosted,
                true,
                200,
                Ok(()),
                None,
            ),
            (
                "cargo private labelled read, reader",
                r,
                false,
                Need::READ,
                Hosted,
                true,
                200,
                Ok(()),
                None,
            ),
            (
                "cargo private labelled read, writer",
                w,
                false,
                Need::READ,
                Hosted,
                true,
                200,
                Ok(()),
                None,
            ),
            (
                "cargo private publish, no grant",
                None,
                false,
                Need::WRITE,
                Hosted,
                false,
                404,
                Err(Refusal::Hidden),
                None,
            ),
        ];
        for &(case, role, anonymous, need, kind, labelled, today, rule, row) in cases {
            let got = decide(&Gate {
                role,
                anonymous,
                need,
                kind,
                labelled,
            });
            assert_eq!(got, rule, "{case}");
            let refused = got.err().map(Refusal::status);
            let today_refused = (today >= 400).then_some(today);
            match row {
                None => assert_eq!(refused, today_refused, "{case}"),
                Some(row) => assert_ne!(refused, today_refused, "{case}: {row} changes nothing"),
            }
        }
    }

    #[test]
    fn a_label_is_read_only_before_a_role_is_looked_at() {
        let g = Gate {
            role: Some(Role::Read),
            anonymous: false,
            need: Need::WRITE,
            kind: Kind::Hosted,
            labelled: true,
        };
        assert_eq!(decide(&g), Err(Refusal::LabelReadOnly));
        let login = Need {
            login: true,
            ..Need::READ
        };
        let g = Gate {
            role: Some(Role::Read),
            anonymous: true,
            need: login,
            kind: Kind::Hosted,
            labelled: false,
        };
        assert_eq!(decide(&g), Err(Refusal::Unauthenticated));
    }
}
