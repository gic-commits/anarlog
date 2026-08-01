import chroma from "chroma-js";
import { type CSSProperties, useMemo } from "react";

import type { SegmentKey, SegmentWord } from "~/stt/live-segment";

export type HighlightSegment = { text: string; isMatch: boolean };

export type SentenceLine = {
  words: SegmentWord[];
  startMs: number;
  endMs: number;
};

type SegmentColorVars = CSSProperties & {
  "--segment-color-light": string;
  "--segment-color-dark": string;
};

export function groupWordsIntoLines(words: SegmentWord[]): SentenceLine[] {
  if (words.length === 0) return [];

  const lines: SentenceLine[] = [];
  let currentLine: SegmentWord[] = [];

  for (const word of words) {
    currentLine.push(word);
    const text = word.text.trim();
    if (text.endsWith(".") || text.endsWith("?") || text.endsWith("!")) {
      lines.push({
        words: currentLine,
        startMs: currentLine[0]!.start_ms,
        endMs: currentLine[currentLine.length - 1]!.end_ms,
      });
      currentLine = [];
    }
  }

  if (currentLine.length > 0) {
    lines.push({
      words: currentLine,
      startMs: currentLine[0]!.start_ms,
      endMs: currentLine[currentLine.length - 1]!.end_ms,
    });
  }

  return lines;
}

export type SegmentBlock = {
  words: SegmentWord[];
  startMs: number;
  endMs: number;
};

export function groupWordsIntoBlocks(words: SegmentWord[]): SegmentBlock[] {
  if (words.length === 0) return [];

  const blocks: SegmentBlock[] = [];
  let current: SegmentWord[] = [];

  for (const word of words) {
    if (word.metadata?.segment_boundary && current.length > 0) {
      blocks.push(makeBlock(current));
      current = [];
    }
    current.push(word);
  }

  if (current.length > 0) {
    blocks.push(makeBlock(current));
  }

  return blocks;
}

function makeBlock(words: SegmentWord[]): SegmentBlock {
  return {
    words,
    startMs: words[0]!.start_ms,
    endMs: words[words.length - 1]!.end_ms,
  };
}

export function getActiveLineIndex(
  words: SegmentWord[],
  offsetMs: number,
  currentMs: number,
): number | null {
  if (currentMs <= 0 || words.length === 0) return null;

  let lineIndex = 0;

  for (const block of groupWordsIntoBlocks(words)) {
    let lineStartMs = block.startMs;

    for (const word of block.words) {
      const text = word.text.trim();
      const closesLine =
        text.endsWith(".") ||
        text.endsWith("?") ||
        text.endsWith("!") ||
        word === block.words[block.words.length - 1];

      if (!closesLine) {
        continue;
      }

      const start = offsetMs + lineStartMs;
      const end = offsetMs + word.end_ms;
      if (currentMs >= start && currentMs <= end) {
        return lineIndex;
      }

      lineIndex += 1;
      const next = block.words[block.words.indexOf(word) + 1];
      lineStartMs = next?.start_ms ?? lineStartMs;
    }
  }

  return null;
}

export function getSegmentColor(
  key: SegmentKey,
  mode: "light" | "dark" = "light",
): string {
  const speakerIndex = key.speaker_index ?? 0;

  const channelPalettes = [
    [10, 25, 0, 340, 15, 350],
    [285, 305, 270, 295, 315, 280],
  ];

  const paletteIndex = key.channel === "RemoteParty" ? 1 : 0;
  const hues = channelPalettes[paletteIndex]!;
  const hue = hues[speakerIndex % hues.length]!;

  return chroma.oklch(mode === "dark" ? 0.72 : 0.55, 0.15, hue).hex();
}

export function getSegmentColorVars(key: SegmentKey): SegmentColorVars {
  return {
    "--segment-color-light": getSegmentColor(key),
    "--segment-color-dark": getSegmentColor(key, "dark"),
  };
}

export function useSegmentColor(key: SegmentKey): string {
  return useMemo(() => getSegmentColor(key), [key]);
}

export function useSegmentColorVars(key: SegmentKey): SegmentColorVars {
  return useMemo(() => getSegmentColorVars(key), [key]);
}
