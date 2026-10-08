//! GitHub-style `:shortcode:` emoji table (Typora renders them inline).
//!
//! No offline emoji crate is vendored, so this is a curated subset of the
//! codes Typora users type most. Lookup is exact-match; the preview swaps
//! the shortcode for the glyph, the live editor styles the token (its
//! source↔visible mapping is byte-exact and cannot host replacements).

/// Glyph for `:name:` (without colons), or `None`.
pub fn lookup(name: &str) -> Option<&'static str> {
    // Linear scan keeps the table readable; single lookup per shortcode,
    // negligible against shaping. Keep entries sorted for review.
    EMOJI.iter().find(|(n, _)| *n == name).map(|(_, c)| *c)
}

/// `:name:` shortcode at byte `from` (must be `:`). Returns the full
/// `:..:` range plus the glyph. Names are `[a-z0-9_+-]{1,32}`.
pub fn find_shortcode(text: &str, from: usize) -> Option<(std::ops::Range<usize>, &'static str)> {
    if text.as_bytes().get(from) != Some(&b':') {
        return None;
    }
    let rest = &text[from + 1..];
    let close = rest.find(':')?;
    if close == 0 || close > 32 {
        return None;
    }
    let name = &rest[..close];
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'+' | b'-'))
    {
        return None;
    }
    lookup(name).map(|c| (from..from + 1 + close + 1, c))
}

const EMOJI: &[(&str, &str)] = &[
    ("+1", "👍"),
    ("-1", "👎"),
    ("100", "💯"),
    ("8ball", "🎱"),
    ("alarm_clock", "⏰"),
    ("angel", "👼"),
    ("angry", "😠"),
    ("arrow_down", "⬇️"),
    ("arrow_left", "⬅️"),
    ("arrow_right", "➡️"),
    ("arrow_up", "⬆️"),
    ("art", "🎨"),
    ("astonished", "😲"),
    ("beers", "🍻"),
    ("bell", "🔔"),
    ("bike", "🚲"),
    ("birthday", "🎂"),
    ("black_heart", "🖤"),
    ("blush", "😊"),
    ("bomb", "💥"),
    ("book", "📖"),
    ("bookmark", "🔖"),
    ("boom", "💥"),
    ("bow", "🙇"),
    ("bug", "🐛"),
    ("bulb", "💡"),
    ("cake", "🍰"),
    ("calendar", "📅"),
    ("camera", "📷"),
    ("car", "🚗"),
    ("cat", "🐱"),
    ("check", "✅"),
    ("cherries", "🍒"),
    ("cherry_blossom", "🌸"),
    ("clap", "👏"),
    ("clock", "🕐"),
    ("coffee", "☕"),
    ("collision", "💥"),
    ("computer", "💻"),
    ("confetti_ball", "🎊"),
    ("confused", "😕"),
    ("cool", "🆒"),
    ("crown", "👑"),
    ("cry", "😢"),
    ("crystal_ball", "🔮"),
    ("cupid", "💘"),
    ("dancer", "💃"),
    ("dart", "🎯"),
    ("dash", "💨"),
    ("disappointed", "😞"),
    ("dizzy", "💫"),
    ("dog", "🐶"),
    ("dollar", "💵"),
    ("doughnut", "🍩"),
    ("dragon", "🐉"),
    ("email", "📧"),
    ("eyes", "👀"),
    ("facepalm", "🤦"),
    ("fearful", "😨"),
    ("fire", "🔥"),
    ("fish", "🐟"),
    ("flags", "🎏"),
    ("flushed", "😳"),
    ("gem", "💎"),
    ("gift", "🎁"),
    ("gift_heart", "💝"),
    ("godmode", "😎"),
    ("green_heart", "💚"),
    ("grin", "😁"),
    ("grinning", "😀"),
    ("hammer", "🔨"),
    ("handshake", "🤝"),
    ("heart", "❤️"),
    ("heart_eyes", "😍"),
    ("heartbeat", "💓"),
    ("herb", "🌿"),
    ("hocho", "🔪"),
    ("honeybee", "🐝"),
    ("hourglass", "⏳"),
    ("hushed", "😯"),
    ("innocent", "😇"),
    ("joy", "😂"),
    ("key", "🔑"),
    ("kissing_heart", "😘"),
    ("laughing", "😆"),
    ("light_bulb", "💡"),
    ("link", "🔗"),
    ("lock", "🔒"),
    ("lollipop", "🍭"),
    ("loud_sound", "🔊"),
    ("love_letter", "💌"),
    ("mag", "🔍"),
    ("mask", "😷"),
    ("memo", "📝"),
    ("metal", "🤘"),
    ("microscope", "🔬"),
    ("milky_way", "🌌"),
    ("moneybag", "💰"),
    ("moon", "🌙"),
    ("muscle", "💪"),
    ("musical_note", "🎵"),
    ("nail_care", "💅"),
    ("neutral_face", "😐"),
    ("new", "🆕"),
    ("newspaper", "📰"),
    ("no_good", "🙅"),
    ("notes", "🎶"),
    ("ok", "🆗"),
    ("ok_hand", "👌"),
    ("open_mouth", "😮"),
    ("package", "📦"),
    ("panda_face", "🐼"),
    ("party", "🎉"),
    ("pencil", "✏️"),
    ("penguin", "🐧"),
    ("pensive", "😔"),
    ("persevere", "😣"),
    ("pig", "🐷"),
    ("pill", "💊"),
    ("pin", "📌"),
    ("pizza", "🍕"),
    ("point_up", "☝️"),
    ("poop", "💩"),
    ("pray", "🙏"),
    ("purple_heart", "💜"),
    ("question", "❓"),
    ("rabbit", "🐰"),
    ("racehorse", "🐎"),
    ("rage", "😡"),
    ("raised_hands", "🙌"),
    ("recycle", "♻️"),
    ("red_circle", "🔴"),
    ("relaxed", "☺️"),
    ("relieved", "😌"),
    ("robot", "🤖"),
    ("rocket", "🚀"),
    ("rose", "🌹"),
    ("sad", "😞"),
    ("scream", "😱"),
    ("shamrock", "☘️"),
    ("ship", "🚢"),
    ("shrug", "🤷"),
    ("sleeping", "😴"),
    ("sleepy", "😪"),
    ("smile", "😄"),
    ("smiley", "😃"),
    ("smiling_imp", "😈"),
    ("smirk", "😏"),
    ("snake", "🐍"),
    ("sob", "😭"),
    ("soccer", "⚽"),
    ("sparkles", "✨"),
    ("sparkling_heart", "💖"),
    ("speak_no_evil", "🙊"),
    ("speech_balloon", "💬"),
    ("star", "⭐"),
    ("star2", "🌟"),
    ("stars", "🌠"),
    ("stuck_out_tongue", "😛"),
    ("stuck_out_tongue_winking_eye", "😜"),
    ("sun", "☀️"),
    ("sunglasses", "😎"),
    ("sweat", "😓"),
    ("sweat_smile", "😅"),
    ("tada", "🎉"),
    ("telescope", "🔭"),
    ("thinking", "🤔"),
    ("thumbsdown", "👎"),
    ("thumbsup", "👍"),
    ("tired_face", "😫"),
    ("tada_party", "🥳"),
    ("tongue", "👅"),
    ("trophy", "🏆"),
    ("tulip", "🌷"),
    ("turtle", "🐢"),
    ("tv", "📺"),
    ("umbrella", "☂️"),
    ("unamused", "😒"),
    ("underage", "🔞"),
    ("unicorn", "🦄"),
    ("unlock", "🔓"),
    ("up", "🆙"),
    ("v", "✌️"),
    ("warning", "⚠️"),
    ("wave", "👋"),
    ("weary", "😩"),
    ("white_check_mark", "✅"),
    ("wink", "😉"),
    ("worried", "😟"),
    ("wrench", "🔧"),
    ("x", "❌"),
    ("yellow_heart", "💛"),
    ("yum", "😋"),
    ("zap", "⚡"),
    ("zzz", "💤"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_codes_resolve() {
        assert_eq!(lookup("smile"), Some("😄"));
        assert_eq!(lookup("tada"), Some("🎉"));
        assert_eq!(lookup("nope_missing"), None);
        assert_eq!(lookup("Smile"), None);
    }

    #[test]
    fn shortcode_scan() {
        assert_eq!(find_shortcode("a :smile: b", 2).unwrap().1, "😄");
        assert!(find_shortcode("a :: b", 2).is_none());
        assert!(find_shortcode("a :nope: b", 2).is_none());
        assert!(find_shortcode("a :SMILE: b", 2).is_none());
    }
}
