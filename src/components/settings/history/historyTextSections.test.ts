import assert from "node:assert";
import { getHistoryTextSections } from "./historyTextSections";

const rawOnly = getHistoryTextSections({
  transcription_text: "raw transcription",
  post_processed_text: null,
});
assert.deepEqual(rawOnly, [
  {
    id: "original",
    translationKey: "settings.history.originalText",
    text: "raw transcription",
  },
]);

const cleaned = getHistoryTextSections({
  transcription_text: "raw transcription\nwith whitespace",
  post_processed_text: "Cleaned transcription.",
});
assert.deepEqual(cleaned, [
  {
    id: "original",
    translationKey: "settings.history.originalText",
    text: "raw transcription\nwith whitespace",
  },
  {
    id: "cleaned",
    translationKey: "settings.history.aiCleanedText",
    text: "Cleaned transcription.",
  },
]);

const emptyCleaned = getHistoryTextSections({
  transcription_text: "raw transcription",
  post_processed_text: "",
});
assert.equal(emptyCleaned.length, 2);
assert.equal(emptyCleaned[1].text, "");

console.log("historyTextSections: all assertions passed");
