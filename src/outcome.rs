//! Outcome 単位の操作(棚卸し)。ここは「満たし済みをどう畳むか / 人手で満たしを表明する」
//! という Outcome の状態にまつわる純粋なドメイン操作を集める(CLI 配線は main が持つ)。
//!
//! # meguri は「実際に満たされているか」を自動照合しない(o28 前提の明文化)
//! satisfied は §5 の通り**事実から導く**が、その事実(受理=acceptance / human 表明)は
//! すべて**人手または明示の accept 由来**で貼られる。meguri は Outcome の statement を読んで
//! 世界が本当にその状態かを再確認しに行くことはない。`outcome done` はまさにこの前提の窓口で、
//! verify 種別に関係なく「達成した」という人間の判断を受理事実として記録するだけである
//! (command でも run/verify を経ずに消し込める。ADR 0002: 受理は Work 由来か人手表明の二択)。

use anyhow::{bail, Result};
use rusqlite::Connection;

use crate::store::{self, Verify};

/// 人手で「達成済み」と表明する(o28)。verify 種別を問わず受理事実を貼る。
/// rollup(まとめ節点)だけは子から導くので手では立てられない。
/// 二重表明を避けるため、既存の人手表明は貼り直す前に消す(冪等)。
pub fn done(conn: &Connection, oid: i64) -> Result<()> {
    let o = store::get_outcome(conn, oid)?;
    if o.verify == Verify::Rollup {
        bail!("o{oid} is a milestone (rollup); it is satisfied by its children, not by hand");
    }
    let repo_id = store::get_intent(conn, o.intent_id)?.repo_id;
    store::remove_human_acceptances(conn, oid)?; // 二重表明を避ける
    store::add_acceptance(conn, oid, None, repo_id, None)?;
    Ok(())
}

/// この Outcome の受理を全部消す(`outcome undone`)。人手表明・work 由来を問わない。
/// 「この Outcome はまだ達成していない」への素直な取り消しであり、誤 accept(work を
/// 誤って accept してしまった場合)のロールバック手段としても使う。
/// 旧データが持つ human_satisfied(sticky)も human 種別なら一緒に落とす。
/// 消した受理の件数を返す(0 なら受理が無かった)。
pub fn undone(conn: &Connection, oid: i64) -> Result<usize> {
    let o = store::get_outcome(conn, oid)?;
    let n = store::remove_all_acceptances(conn, oid)?;
    // accept 済み Work を verified に戻す(受理を消したので状態を整える。backfill にも拾われない)。
    store::unaccept_works(conn, oid)?;
    if o.verify == Verify::Human {
        store::set_human_satisfied(conn, oid, false)?; // 旧データの sticky も消す
    }
    Ok(n)
}

#[cfg(test)]
fn mem() -> Connection {
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch(crate::db::SCHEMA).unwrap();
    c
}

#[cfg(test)]
fn accepted(c: &Connection) -> std::collections::HashSet<i64> {
    store::accepted_outcome_ids(c).unwrap()
}

/// 人手 done が rollup 以外に一般化されている(command でも run 抜きで満たせる)ことと、
/// 冪等・rollup 拒否を固定する。テスト名は `outcome::mark_done`(DoD の検証点)。
#[cfg(test)]
#[test]
fn mark_done() {
    use crate::derive::{states, State};

    let c = mem();
    let iid = store::add_intent(&c, "I", "").unwrap();
    // command 種別でも run/verify を経ずに人手で消し込める(rollup 以外に一般化)。
    let cmd = store::add_outcome(&c, iid, "cmd", "", &Verify::Command("true".into()), &[]).unwrap();
    let hum = store::add_outcome(&c, iid, "hum", "", &Verify::Human, &[]).unwrap();
    let roll = store::add_outcome(&c, iid, "roll", "", &Verify::Rollup, &[cmd]).unwrap();

    // done 前: 受理事実なし。
    assert!(!accepted(&c).contains(&cmd));

    // command を人手 done → 受理事実が貼られ satisfied になる。
    done(&c, cmd).unwrap();
    assert!(accepted(&c).contains(&cmd));
    let outs = store::list_outcomes(&c, Some(iid)).unwrap();
    assert_eq!(states(&outs, &accepted(&c))[&cmd], State::Satisfied);

    // human も同じ窓口で done できる。
    done(&c, hum).unwrap();
    assert!(accepted(&c).contains(&hum));

    // 冪等: 二重 done しても受理は 1 件のまま(undone 1 回で完全に消える)。
    done(&c, cmd).unwrap();
    assert_eq!(undone(&c, cmd).unwrap(), 1);
    assert!(!accepted(&c).contains(&cmd));

    // rollup は手で立てられない(子から導く)。
    assert!(done(&c, roll).is_err());
}

/// 誤 accept のロールバック: `undone` は人手表明だけでなく work 由来の受理も含めて
/// その Outcome の受理を全部消す(o29 の DoD)。
#[cfg(test)]
#[test]
fn undone_clears_all_acceptances() {
    let c = mem();
    let iid = store::add_intent(&c, "I", "").unwrap();
    let oid = store::add_outcome(&c, iid, "s", "", &Verify::Command("true".into()), &[]).unwrap();
    let wid = store::add_work(&c, oid, "obj", "ai").unwrap();

    // work 由来の受理(誤 accept を模す)と人手表明の両方を貼る。
    store::add_acceptance(&c, oid, Some(wid), None, Some("deadbeef")).unwrap();
    done(&c, oid).unwrap();
    assert!(accepted(&c).contains(&oid));

    // undone で両方消える(旧実装は work 由来の受理を残していた)。
    assert_eq!(undone(&c, oid).unwrap(), 2);
    assert!(!accepted(&c).contains(&oid));

    // 受理が無い状態で undone しても 0 件。
    assert_eq!(undone(&c, oid).unwrap(), 0);
}
