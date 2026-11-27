// SPDX-License-Identifier: EUPL-1.2
//! `Debug` for extension messages that carry message text or idempotency
//! keys: only their lengths are shown.
use super::{ChatEdit, ChatSend, Message, Preview, Rule, ShareLink, ShareLinkCreate};
use std::fmt;

struct Length(usize);

impl fmt::Debug for Length {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{} bytes redacted>", self.0)
    }
}

impl fmt::Debug for Preview {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let image = self.image.as_ref().map_or(0, |image| image.data.len());
        f.debug_struct("Preview")
            .field("version", &self.version)
            .field("provider", &self.provider)
            .field("url", &Length(self.url.len()))
            .field("title", &Length(self.title.len()))
            .field("description", &Length(self.description.len()))
            .field("image", &image)
            .finish()
    }
}

impl fmt::Debug for ChatSend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatSend")
            .field("request_id", &self.request_id)
            .field("conversation", &self.conversation)
            .field("idempotency_key", &Length(self.idempotency_key.len()))
            .field("format", &self.format)
            .field("content", &Length(self.content.len()))
            .field("reply_to", &self.reply_to)
            .field("preview", &self.preview)
            .finish()
    }
}

impl fmt::Debug for ChatEdit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatEdit")
            .field("request_id", &self.request_id)
            .field("conversation", &self.conversation)
            .field("target", &self.target)
            .field("format", &self.format)
            .field("content", &Length(self.content.len()))
            .finish()
    }
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Message")
            .field("seq", &self.seq)
            .field("kind", &self.kind)
            .field("target", &self.target)
            .field("author", &self.author)
            .field("format", &self.format)
            .field("content", &Length(self.content.len()))
            .field("reply_to", &self.reply_to)
            .field("sent_ms", &self.sent_ms)
            .field("edited_ms", &self.edited_ms)
            .field("deleted_ms", &self.deleted_ms)
            .field("delete_reason", &self.delete_reason)
            .field("edits", &self.edits)
            .field("mls", &Length(self.mls.len()))
            .field("preview", &self.preview)
            .finish()
    }
}

// Share links: the token and the access token open channels.
impl fmt::Debug for ShareLinkCreate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShareLinkCreate")
            .field("request_id", &self.request_id)
            .field("channel", &self.channel)
            .field("expires_at", &self.expires_at)
            .field("access_token", &Length(self.access_token.len()))
            .finish()
    }
}

impl fmt::Debug for ShareLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShareLink")
            .field("request_id", &self.request_id)
            .field("token", &Length(self.token.len()))
            .field("channel", &self.channel)
            .field("expires_at", &self.expires_at)
            .field("access_token", &Length(self.access_token.len()))
            .field("web_host", &self.web_host)
            .finish()
    }
}

impl fmt::Debug for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // A group of "#name" is an access token, i.e. a channel password.
        f.debug_struct("Rule")
            .field("apply_here", &self.apply_here)
            .field("apply_subs", &self.apply_subs)
            .field("user_id", &self.user_id)
            .field("group", &Length(self.group.len()))
            .field("grant", &self.grant)
            .field("deny", &self.deny)
            .finish()
    }
}
