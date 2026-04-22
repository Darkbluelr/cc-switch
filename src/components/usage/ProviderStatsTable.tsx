import { useTranslation } from "react-i18next";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { useProviderStats } from "@/lib/query/usage";
import { fmtUsd } from "./format";
import type { UsageRangeSelection } from "@/types/usage";
import { cn } from "@/lib/utils";

interface ProviderStatsTableProps {
  range: UsageRangeSelection;
  appType?: string;
  refreshIntervalMs: number;
}

export function ProviderStatsTable({
  range,
  appType,
  refreshIntervalMs,
}: ProviderStatsTableProps) {
  const { t } = useTranslation();
  const { data: stats, isLoading } = useProviderStats(range, appType, {
    refetchInterval: refreshIntervalMs > 0 ? refreshIntervalMs : false,
  });

  if (isLoading) {
    return <div className="h-[400px] animate-pulse rounded bg-gray-100" />;
  }

  return (
    <div className="rounded-lg border border-border/50 bg-card/40 backdrop-blur-sm overflow-hidden">
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>{t("usage.provider", "Provider")}</TableHead>
            <TableHead className="text-right">
              {t("usage.requests", "请求数")}
            </TableHead>
            <TableHead className="text-right">
              {t("usage.tokens", "Tokens")}
            </TableHead>
            <TableHead className="text-right">
              {t("usage.cost", "成本")}
            </TableHead>
            <TableHead className="text-right">
              {t("usage.successRate", "成功率")}
            </TableHead>
            <TableHead
              className="text-right"
              title={t(
                "usage.cacheHitRateHint",
                [
                  "按 token 加权的命中率：cache_read / 总 prompt tokens。",
                  "这是业界惯例（Anthropic / OpenAI / LiteLLM 都这么算）。",
                  "",
                  "公式按 app 类型不同：",
                  "  Codex:  cache_read / input_tokens（input 已含 cached）",
                  "  Claude: cache_read / (input + cache_read + cache_creation)",
                  "",
                  "⚠ 不等于美元成本节省率 —— cache read 仍要计费（Claude 约 10%，OpenAI 约 50%），",
                  "实际节省率 ≤ 本数值。",
                ].join("\n"),
              )}
            >
              {t("usage.cacheHitRate", "缓存命中率")}
            </TableHead>
            <TableHead className="text-right">
              {t("usage.avgLatency", "平均延迟")}
            </TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {stats?.length === 0 ? (
            <TableRow>
              <TableCell
                colSpan={7}
                className="text-center text-muted-foreground"
              >
                {t("usage.noData", "暂无数据")}
              </TableCell>
            </TableRow>
          ) : (
            stats?.map((stat) => (
              <TableRow key={`${stat.appType}:${stat.providerId}`}>
                <TableCell className="font-medium">
                  {stat.providerName}
                </TableCell>
                <TableCell className="text-right">
                  {stat.requestCount.toLocaleString()}
                </TableCell>
                <TableCell className="text-right">
                  {stat.totalTokens.toLocaleString()}
                </TableCell>
                <TableCell className="text-right">
                  {fmtUsd(stat.totalCost, 4)}
                </TableCell>
                <TableCell className="text-right">
                  {stat.successRate.toFixed(1)}%
                </TableCell>
                <TableCell
                  className={cn(
                    "text-right font-mono text-xs",
                    cacheHitToneClass(stat.cacheHitRate),
                  )}
                >
                  {formatCacheHit(stat.cacheHitRate)}
                </TableCell>
                <TableCell className="text-right">
                  {stat.avgLatencyMs}ms
                </TableCell>
              </TableRow>
            ))
          )}
        </TableBody>
      </Table>
    </div>
  );
}

/** 命中率 null（无样本）→ "—"；否则百分比保留 1 位小数 */
function formatCacheHit(rate: number | null): string {
  if (rate === null || Number.isNaN(rate)) return "—";
  return `${(rate * 100).toFixed(1)}%`;
}

/** 命中率按阈值着色（绿≥70%, 黄 30-70%, 红<30%）*/
function cacheHitToneClass(rate: number | null): string {
  if (rate === null) return "text-muted-foreground";
  if (rate >= 0.7) return "text-emerald-600 dark:text-emerald-400";
  if (rate >= 0.3) return "text-amber-600 dark:text-amber-400";
  return "text-red-600 dark:text-red-400";
}
