//! The games an account can play: the tiles of the home and browse pages,
//! kept when `getProductEntitlementStatuses` says `ENTITLED`, in the order
//! the home screen shows them: continue playing, recently added, then all
//! of them by title.

use std::collections::HashSet;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::pages::walk;

/// Ids per entitlement call (25 is accepted, 50 fails).
pub const ENTITLEMENT_BATCH: usize = 25;
pub const HOME_PAGE: &str = "landing_home";
pub const BROWSE_PAGE: &str = "multistate_browse_results";
const CONTINUE_ROW: &str = "collection_recently_played_v2";
const RECENT_ROW: &str = "collection_recently_added_luna_standard";

/// A game of a catalog tile.
#[derive(Clone, PartialEq, Debug, Deserialize)]
pub struct Game {
    #[serde(rename = "gameId")]
    pub id: String,
    pub title: String,
    #[serde(default, rename = "publishersText")]
    pub publisher: Option<String>,
    #[serde(default, rename = "genreTags")]
    pub genres: Vec<String>,
    #[serde(default, rename = "imagePortrait")]
    pub portrait: Option<String>,
    #[serde(default, rename = "imageLandscape")]
    pub landscape: Option<String>,
}

/// The playable games, and how many of the catalog are not.
#[derive(Clone, PartialEq, Debug)]
pub struct Catalog {
    pub games: Vec<Game>,
    pub hidden: usize,
}

/// The games of every tile of a page, in page order.
pub fn games_in(page: &Value) -> Vec<Game> {
    let mut out = Vec::new();
    walk(page, &mut |m| {
        let tile = m
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| t.starts_with("GAME_TILE"));
        let data = m.get("presentationData").and_then(Value::as_str);
        if let (true, Some(d)) = (tile, data) {
            out.extend(serde_json::from_str::<Game>(d).ok());
        }
    });
    out
}

/// The games of the widget `id` of a page.
fn row(page: &Value, id: &str) -> Vec<Game> {
    let mut found = None;
    find_widget(page, id, &mut found);
    found.map(games_in).unwrap_or_default()
}

fn find_widget<'a>(v: &'a Value, id: &str, found: &mut Option<&'a Value>) {
    if found.is_some() {
        return;
    }
    match v {
        Value::Object(m) if m.get("id").and_then(Value::as_str) == Some(id) => *found = Some(v),
        Value::Object(m) => m.values().for_each(|c| find_widget(c, id, found)),
        Value::Array(a) => a.iter().for_each(|c| find_widget(c, id, found)),
        _ => {}
    }
}

/// Every id of the pages once, in page order (for the entitlement calls).
pub fn all_ids(pages: &[&Value]) -> Vec<String> {
    let mut seen = HashSet::new();
    pages
        .iter()
        .flat_map(|p| games_in(p))
        .filter(|g| seen.insert(g.id.clone()))
        .map(|g| g.id)
        .collect()
}

pub fn entitlement_request(ids: &[String]) -> Value {
    json!({"productIdList": ids, "stage": "Release"})
}

/// The ids an entitlement reply allows.
pub fn entitled(reply: &Value) -> HashSet<String> {
    reply["productEntitlementStatuses"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["status"] == "ENTITLED")
        .filter_map(|e| e["productId"].as_str().map(str::to_owned))
        .collect()
}

/// The playable games once each: continue playing, recently added, then
/// the rest of the catalog by title.
pub fn build(home: &Value, browse: &Value, ok: &HashSet<String>) -> Catalog {
    let mut all = games_in(browse);
    let mut seen = HashSet::new();
    all.retain(|g| seen.insert(g.id.clone()));
    all.sort_by_cached_key(|g| g.title.to_lowercase());
    let hidden = all.iter().filter(|g| !ok.contains(&g.id)).count();
    let mut shown = HashSet::new();
    let games = row(home, CONTINUE_ROW)
        .into_iter()
        .chain(row(home, RECENT_ROW))
        .chain(all)
        .filter(|g| ok.contains(&g.id) && shown.insert(g.id.clone()))
        .collect();
    Catalog { games, hidden }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(kind: &str, id: &str, title: &str) -> Value {
        let d = json!({"gameId": id, "title": title, "genreTags": ["Racing"]}).to_string();
        json!({"type": kind, "presentationData": d})
    }

    fn ids(l: &[&str]) -> HashSet<String> {
        l.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn tiles_are_read_broken_ones_skipped() {
        let page = json!({"w": [tile("GAME_TILE", "a", "A"), tile("GAME_TILE_VERTICAL", "b", "B"),
            {"type": "GAME_TILE", "presentationData": "not json"},
            {"type": "GAME_TILE", "presentationData": "{\"gameId\":\"x\"}"},
            tile("GENRE_TILE", "g", "Genre")]});
        let g = games_in(&page);
        assert_eq!(g.len(), 2);
        assert_eq!(g[1].genres, vec!["Racing".to_string()]);
    }

    #[test]
    fn entitlements() {
        let r = json!({"productEntitlementStatuses": [
            {"productId": "a", "status": "ENTITLED"},
            {"productId": "b", "status": "NOT_ENTITLED"}]});
        assert_eq!(entitled(&r), ids(&["a"]));
        assert!(entitled(&json!({})).is_empty());
        assert_eq!(entitlement_request(&["a".into()])["stage"], "Release");
    }

    #[test]
    fn rows_first_locked_hidden_each_once() {
        let home = json!({"w": [{"id": CONTINUE_ROW, "widgets": [
            tile("GAME_TILE_VERTICAL", "locked", "Dead Space"),
            tile("GAME_TILE_VERTICAL", "z", "Zelda")]}]});
        let browse = json!({"w": [tile("GAME_TILE", "z", "Zelda"),
            tile("GAME_TILE", "locked", "Dead Space"), tile("GAME_TILE", "a", "alan wake"),
            tile("GAME_TILE", "a", "alan wake")]});
        let c = build(&home, &browse, &ids(&["z", "a"]));
        assert_eq!(c.hidden, 1);
        let order: Vec<&str> = c.games.iter().map(|g| g.id.as_str()).collect();
        assert_eq!(order, vec!["z", "a"]);
    }

    #[test]
    fn ids_once_across_pages() {
        let home = json!([tile("GAME_TILE", "a", "A"), tile("GAME_TILE", "b", "B")]);
        let browse = json!([tile("GAME_TILE", "b", "B"), tile("GAME_TILE", "c", "C")]);
        assert_eq!(all_ids(&[&home, &browse]), vec!["a", "b", "c"]);
    }
}
