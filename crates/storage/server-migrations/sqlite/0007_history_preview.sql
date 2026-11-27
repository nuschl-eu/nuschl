-- The carried preview of a message (docs/link-previews.md): the card the
-- sender fetched, as JSON, stored with the text it belongs to. An edit or a
-- deletion clears it.
ALTER TABLE history_messages ADD COLUMN preview TEXT;
