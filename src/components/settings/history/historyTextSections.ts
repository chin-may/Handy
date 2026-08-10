export type HistoryTextSection = {
  id: "original" | "cleaned";
  translationKey:
    | "settings.history.originalText"
    | "settings.history.aiCleanedText";
  text: string;
};

type HistoryTextSource = {
  transcription_text: string;
  post_processed_text: string | null;
};

/**
 * Keep the original transcription visible whenever post-processing produced a
 * result. `null` means no result was stored; an empty result is still a result
 * and is deliberately shown so history accurately reflects what AI returned.
 */
export function getHistoryTextSections(
  entry: HistoryTextSource,
): HistoryTextSection[] {
  const original: HistoryTextSection = {
    id: "original",
    translationKey: "settings.history.originalText",
    text: entry.transcription_text,
  };

  if (entry.post_processed_text === null) {
    return [original];
  }

  return [
    original,
    {
      id: "cleaned",
      translationKey: "settings.history.aiCleanedText",
      text: entry.post_processed_text,
    },
  ];
}
