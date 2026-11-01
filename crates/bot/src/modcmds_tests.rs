//! Tests of `modcmds` (own file to keep modcmds.rs under 1000 lines).
use super::*;
use crate::prefix::parse;

fn pf(s: &str, resolved: Option<u64>) -> Result<ModCmd, String> {
    from_prefix(&parse(s).unwrap(), resolved)
}

#[test]
fn prefix_parsing() {
    let big = 123_456_789_012_345_678u64;
    assert_eq!(
        pf(&format!("unban {big}"), None),
        Ok(ModCmd::Unban { user: big })
    );
    assert!(pf("unban", None).is_err());
    assert_eq!(
        pf("untimeout bob", Some(5_000_000)),
        Ok(ModCmd::Untimeout { user: 5_000_000 })
    );
    assert_eq!(pf("unwarn 7", None), Ok(ModCmd::Unwarn { case: 7 }));
    assert_eq!(
        pf(&format!("purge 50 {big}"), None),
        Ok(ModCmd::Purge {
            count: 50,
            user: Some(big),
            from: None,
            filters: vec![]
        })
    );
    assert!(pf("purge 5000", None).is_err());
    assert_eq!(
        pf("slowmode 10s", None),
        Ok(ModCmd::Slowmode {
            secs: 10,
            channels: vec![]
        })
    );
    assert_eq!(
        pf("slowmode off", None),
        Ok(ModCmd::Slowmode {
            secs: 0,
            channels: vec![]
        })
    );
    assert!(pf("slowmode 7h", None).is_err());
    assert_eq!(
        pf("sm <#5000000> <#6000000> 1h", None),
        Ok(ModCmd::Slowmode {
            secs: 3600,
            channels: vec![5_000_000, 6_000_000]
        })
    );
    assert_eq!(
        pf("cases modify 3 ?r typo", None),
        Ok(ModCmd::CasesEdit {
            case: 3,
            reason: "typo".into()
        })
    );
    assert_eq!(
        pf("cases 3", None),
        Ok(ModCmd::CasesView {
            case: Some(3),
            user: None,
            by: None,
            kind: None
        })
    );
    assert_eq!(
        pf("cases", None),
        Ok(ModCmd::CasesView {
            case: None,
            user: None,
            by: None,
            kind: None
        })
    );
    assert_eq!(
        pf("editc 3 fixed typo", None),
        Ok(ModCmd::CasesEdit {
            case: 3,
            reason: "fixed typo".into()
        })
    );
    assert!(pf("editreason 3", None).is_err());
    assert_eq!(
        pf("caseoff 4", None),
        Ok(ModCmd::CaseSet {
            case: 4,
            active: false
        })
    );
    assert_eq!(
        pf("opencase 4", None),
        Ok(ModCmd::CaseSet {
            case: 4,
            active: true
        })
    );
    assert_eq!(
        pf("clearwarns 5000000", None),
        Ok(ModCmd::ClearWarns { user: 5_000_000 })
    );
}

#[test]
fn multi_target_and_purge_aliases() {
    let cmd = ModCmd::Unban { user: 1 };
    assert_eq!(cmd.with_user(2), Some(ModCmd::Unban { user: 2 }));
    assert_eq!(
        ModCmd::Slowmode {
            secs: 1,
            channels: vec![]
        }
        .with_user(2),
        None
    );
    let big = 123_456_789_012_345_678u64;
    let c = pf(&format!("bc 20 {big}"), None).unwrap();
    let ModCmd::Purge { filters, .. } = c else {
        panic!()
    };
    assert_eq!(filters, vec![purge::Filter::Bots]);
    assert_eq!(
        purge::Filter::parse("-sus", None),
        Ok(purge::Filter::Suspicious)
    );
    assert_eq!(purge::Filter::parse("@", None), Ok(purge::Filter::Mentions));
    assert_eq!(
        purge::Filter::parse("=", Some("Hi")),
        Ok(purge::Filter::Equals("hi".into()))
    );
}

#[test]
fn note_prefix() {
    let big = 123_456_789_012_345_678u64;
    assert_eq!(
        pf(&format!("note add {big} ?r rude in vc"), None),
        Ok(ModCmd::NoteAdd {
            user: big,
            text: "rude in vc".into()
        })
    );
    assert!(pf(&format!("note add {big}"), None).is_err()); // text required
    assert_eq!(
        pf("note remove q413p9", None),
        Ok(ModCmd::NoteRemove {
            id: "q413p9".into(),
            user: None
        })
    );
    assert_eq!(
        pf(&format!("note {big}"), None),
        Ok(ModCmd::NoteList {
            user: Some(big),
            id: None,
            by: None
        })
    );
    assert_eq!(
        ModCmd::NoteList {
            user: None,
            id: None,
            by: None
        }
        .perm(),
        "notes_view"
    );
}

#[test]
fn dehoist_alias() {
    assert_eq!(
        pf("dehoist 5000000", None),
        Ok(ModCmd::Sanitize { user: 5_000_000 })
    );
}
