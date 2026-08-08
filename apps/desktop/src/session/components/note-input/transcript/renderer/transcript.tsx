import { useVirtualizer } from "@tanstack/react-virtual";
import { memo, useCallback, useEffect, useMemo } from "react";

import { cn } from "@hypr/utils";

import { useSearch } from "../../search/context";
import { useRenderedTranscriptData, useTranscriptOffset } from "./data-hooks";
import {
  EMPTY_TRANSCRIPT_SEARCH,
  SegmentRenderer,
  type TranscriptSearchRenderState,
} from "./segment";
import {
  createSegmentKey,
  segmentsShallowEqual,
  useStableSegments,
} from "./segment-hooks";
import { estimateSegmentHeight } from "./viewport-hooks";

import {
  mergeRenderedAndLiveSegments,
  type Segment,
  type SegmentWord,
} from "~/stt/live-segment";
import { useTranscriptLabelContext } from "~/stt/queries";
import { SpeakerLabelManager } from "~/stt/segment/shared";
import { isTranscriptWordSeekable } from "~/stt/timing";

export function RenderTranscript({
  scrollElement,
  isLastTranscript,
  shouldScrollToEnd,
  transcriptId,
  liveSegments,
  currentMs,
  seek,
  startPlayback,
  audioExists,
  seekInteractionCount,
}: {
  scrollElement: HTMLDivElement | null;
  isLastTranscript: boolean;
  shouldScrollToEnd: boolean;
  transcriptId: string;
  liveSegments: Segment[];
  currentMs: number;
  seek: (sec: number) => void;
  startPlayback: () => void;
  audioExists: boolean;
  seekInteractionCount: number;
}) {
  const { maxSpeakerNumber, segments: storedSegments } =
    useRenderedTranscriptData(transcriptId);
  const mergedSegments = useMemo(
    () => mergeRenderedAndLiveSegments(storedSegments, liveSegments),
    [liveSegments, storedSegments],
  );
  const segments = useStableSegments(mergedSegments);
  const offsetMs = useTranscriptOffset(transcriptId);

  if (segments.length === 0) {
    return null;
  }

  return (
    <SegmentsList
      segments={segments}
      scrollElement={scrollElement}
      transcriptId={transcriptId}
      offsetMs={offsetMs}
      shouldScrollToEnd={isLastTranscript && shouldScrollToEnd}
      currentMs={currentMs}
      seek={seek}
      startPlayback={startPlayback}
      audioExists={audioExists}
      maxSpeakerNumber={maxSpeakerNumber}
      seekInteractionCount={seekInteractionCount}
    />
  );
}

const SegmentsList = memo(
  ({
    segments,
    scrollElement,
    transcriptId,
    offsetMs,
    shouldScrollToEnd,
    currentMs,
    seek,
    startPlayback,
    audioExists,
    maxSpeakerNumber,
    seekInteractionCount,
  }: {
    segments: Segment[];
    scrollElement: HTMLDivElement | null;
    transcriptId: string;
    offsetMs: number;
    shouldScrollToEnd: boolean;
    currentMs: number;
    seek: (sec: number) => void;
    startPlayback: () => void;
    audioExists: boolean;
    maxSpeakerNumber?: number;
    seekInteractionCount: number;
  }) => {
    const labelContext = useTranscriptLabelContext(transcriptId);
    const search = useSearch();
    const speakerLabelManager = useMemo(() => {
      return labelContext
        ? SpeakerLabelManager.fromSegments(
            segments,
            labelContext,
            maxSpeakerNumber,
          )
        : new SpeakerLabelManager();
    }, [labelContext, maxSpeakerNumber, segments]);
    const transcriptSearch = useMemo<TranscriptSearchRenderState>(() => {
      const query = search?.query.trim() ?? "";
      if (!search?.isVisible || !query) {
        return EMPTY_TRANSCRIPT_SEARCH;
      }

      return {
        query,
        activeMatchId: search.activeMatchId,
        caseSensitive: search.caseSensitive,
        wholeWord: search.wholeWord,
      };
    }, [
      search?.activeMatchId,
      search?.caseSensitive,
      search?.isVisible,
      search?.query,
      search?.wholeWord,
    ]);

    const seekAndPlay = useCallback(
      (word: SegmentWord) => {
        if (audioExists && isTranscriptWordSeekable(word)) {
          seek((offsetMs + word.start_ms) / 1000);
          startPlayback();
        }
      },
      [audioExists, offsetMs, seek, startPlayback],
    );

    useEffect(() => {
      if (!scrollElement || !shouldScrollToEnd) {
        return;
      }
      const raf = requestAnimationFrame(() => {
        scrollElement.scrollTo({
          top: scrollElement.scrollHeight,
          behavior: "auto",
        });
      });
      return () => cancelAnimationFrame(raf);
    }, [scrollElement, segments.length, shouldScrollToEnd]);

    // Window very long transcripts so we only mount segments near the
    // viewport instead of rendering every word of a multi-hour meeting.
    const windowing = segments.length > 200;

    const virtualizer = useVirtualizer({
      count: segments.length,
      getScrollElement: () => scrollElement,
      estimateSize: (index) => estimateSegmentHeight(segments[index]!),
      overscan: 8,
      enabled: windowing,
      // Avoid `flushSync` inside React render lifecycle (React 19):
      // sync onChange from scrollToIndex can otherwise warn/error.
      useFlushSync: false,
    });

    // When the user clicks/drags the audio timeline, jump the transcript to
    // the segment that contains the new playback position.
    useEffect(() => {
      if (!windowing || !scrollElement || seekInteractionCount === 0) {
        return;
      }
      const seekMs = currentMs;
      if (seekMs <= 0 || segments.length === 0) {
        return;
      }

      // Segments are sorted by start_ms; find the first whose end >= seekMs.
      let lo = 0;
      let hi = segments.length - 1;
      while (lo < hi) {
        const mid = (lo + hi) >> 1;
        const midWords = segments[mid]!.words;
        const segEnd = offsetMs + (midWords[midWords.length - 1]?.end_ms ?? 0);
        if (segEnd < seekMs) {
          lo = mid + 1;
        } else {
          hi = mid;
        }
      }
      const targetIndex = lo;
      const currentStart =
        offsetMs + (segments[targetIndex]!.words[0]?.start_ms ?? 0);
      // Only jump if the target segment is meaningfully far from the
      // viewport so we don't fight the user's own scrolling.
      const items = virtualizer.getVirtualItems();
      const inView =
        items.length > 0 &&
        targetIndex >= items[0]!.index &&
        targetIndex <= items[items.length - 1]!.index;
      if (!inView || Math.abs(currentStart - seekMs) > 5_000) {
        virtualizer.scrollToIndex(targetIndex, { align: "center" });

        // After the virtualizer scrolls (based on estimated sizes), the target
        // segment mounts and gets measured. Then snap to its real DOM position
        // so the jump is accurate regardless of estimate/measurement drift —
        // this matches the "look up words near the timestamp" mental model.
        window.setTimeout(() => {
          const targetEl = scrollElement.querySelector<HTMLElement>(
            `[data-index="${targetIndex}"]`,
          );
          if (targetEl) {
            targetEl.scrollIntoView({ block: "center" });
          }
        }, 120);
      }
    }, [
      seekInteractionCount,
      currentMs,
      offsetMs,
      scrollElement,
      segments,
      virtualizer,
      windowing,
    ]);

    // For non-windowed transcripts, locate the current line on seek using
    // the existing data-line-current marker (played back or jumped to).
    useEffect(() => {
      if (windowing || !scrollElement || seekInteractionCount === 0) {
        return;
      }
      const currentLine = scrollElement.querySelector<HTMLElement>(
        "[data-line-current='true']",
      );
      if (currentLine) {
        currentLine.scrollIntoView({ block: "center" });
      }
    }, [seekInteractionCount, currentMs, scrollElement, windowing]);

    if (!windowing) {
      return (
        <div>
          {segments.map((segment, index) => (
            <div
              key={createSegmentKey(segment, transcriptId, index)}
              className={cn([index > 0 && "pt-4"])}
            >
              <SegmentRenderer
                segment={segment}
                offsetMs={offsetMs}
                transcriptId={transcriptId}
                speakerLabelManager={speakerLabelManager}
                currentMs={currentMs}
                seekAndPlay={seekAndPlay}
                audioExists={audioExists}
                search={transcriptSearch}
              />
            </div>
          ))}
        </div>
      );
    }

    const items = virtualizer.getVirtualItems();

    return (
      <div
        style={{
          height: `${virtualizer.getTotalSize()}px`,
          width: "100%",
          position: "relative",
        }}
      >
        {items.map((virtualItem) => {
          const segment = segments[virtualItem.index]!;
          return (
            <div
              key={createSegmentKey(segment, transcriptId, virtualItem.index)}
              ref={virtualizer.measureElement}
              data-index={virtualItem.index}
              style={{
                position: "absolute",
                top: virtualItem.start,
                left: 0,
                width: "100%",
              }}
              className={cn([virtualItem.index > 0 && "pt-4"])}
            >
              <SegmentRenderer
                segment={segment}
                offsetMs={offsetMs}
                transcriptId={transcriptId}
                speakerLabelManager={speakerLabelManager}
                currentMs={currentMs}
                seekAndPlay={seekAndPlay}
                audioExists={audioExists}
                search={transcriptSearch}
              />
            </div>
          );
        })}
      </div>
    );
  },
  (prevProps, nextProps) => {
    return (
      prevProps.transcriptId === nextProps.transcriptId &&
      prevProps.scrollElement === nextProps.scrollElement &&
      prevProps.offsetMs === nextProps.offsetMs &&
      prevProps.shouldScrollToEnd === nextProps.shouldScrollToEnd &&
      prevProps.currentMs === nextProps.currentMs &&
      prevProps.audioExists === nextProps.audioExists &&
      prevProps.maxSpeakerNumber === nextProps.maxSpeakerNumber &&
      prevProps.seek === nextProps.seek &&
      prevProps.startPlayback === nextProps.startPlayback &&
      prevProps.seekInteractionCount === nextProps.seekInteractionCount &&
      segmentsShallowEqual(prevProps.segments, nextProps.segments)
    );
  },
);
