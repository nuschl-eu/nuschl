-- SPDX-License-Identifier: EUPL-1.2
-- Search text is now folded: accents become their base letters, and the
-- characters of a script written without spaces stand apart, so a word inside
-- a run matches. The backfill rewrites the stored text, so this empties the
-- index. The text fill runs newest first in the background, and the index pass
-- follows it; search reports that it is still indexing until both are done.
DELETE FROM message_search;
-- Everything at or above `below` counts as indexed: put it above every
-- message, so the index pass walks the rewritten text from the newest row.
UPDATE search_backfill SET below = (SELECT COALESCE(MAX(id), 0) + 1 FROM messages);
