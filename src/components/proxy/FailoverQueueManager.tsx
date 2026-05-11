/**
 * 故障转移队列管理组件
 *
 * 允许用户管理代理模式下的故障转移队列，支持：
 * - 添加/移除供应商
 * - 队列顺序按 failover_tier 分层，其内基于首页供应商列表的 sort_index
 */

import { useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { Plus, Trash2, Loader2, Info, AlertTriangle } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { Alert, AlertDescription } from "@/components/ui/alert";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { cn } from "@/lib/utils";
import type { FailoverQueueItem } from "@/types/proxy";
import type { AppId } from "@/lib/api";
import {
  useFailoverQueue,
  useAvailableProvidersForFailover,
  useAddToFailoverQueue,
  useRemoveFromFailoverQueue,
  useSetFailoverTier,
  useAutoFailoverEnabled,
  useSetAutoFailoverEnabled,
  useProviderHealthMetrics,
} from "@/lib/query/failover";
import type { ProviderHealthMetricsView } from "@/types/proxy";

interface FailoverQueueManagerProps {
  appType: AppId;
  disabled?: boolean;
}

export function FailoverQueueManager({
  appType,
  disabled = false,
}: FailoverQueueManagerProps) {
  const { t } = useTranslation();
  const [selectedProviderId, setSelectedProviderId] = useState<string>("");
  const [selectedTier, setSelectedTier] = useState<string>("1");

  // 故障转移开关状态（每个应用独立）
  const { data: isFailoverEnabled = false } = useAutoFailoverEnabled(appType);
  const setFailoverEnabled = useSetAutoFailoverEnabled();

  // 查询数据
  const {
    data: queue,
    isLoading: isQueueLoading,
    error: queueError,
  } = useFailoverQueue(appType);
  const { data: availableProviders, isLoading: isProvidersLoading } =
    useAvailableProvidersForFailover(appType);

  // Per-provider 健康指标（默认 30 分钟窗口，15s 轮询刷新）
  const { data: metricsList } = useProviderHealthMetrics(appType);
  const metricsByProvider = new Map<string, ProviderHealthMetricsView>(
    (metricsList ?? []).map((m) => [m.providerId, m]),
  );

  // Mutations
  const addToQueue = useAddToFailoverQueue();
  const removeFromQueue = useRemoveFromFailoverQueue();
  const setFailoverTier = useSetFailoverTier();

  // 切换故障转移开关
  const handleToggleFailover = (enabled: boolean) => {
    setFailoverEnabled.mutate({ appType, enabled });
  };

  // 添加供应商到队列
  const handleAddProvider = async () => {
    if (!selectedProviderId) return;

    try {
      await addToQueue.mutateAsync({
        appType,
        providerId: selectedProviderId,
      });

      const tier = Number(selectedTier);
      if (tier && tier !== 1) {
        await setFailoverTier.mutateAsync({
          appType,
          providerId: selectedProviderId,
          tier,
        });
      }

      setSelectedProviderId("");
      setSelectedTier("1");
      toast.success(
        t("proxy.failoverQueue.addSuccess", "已添加到故障转移队列"),
        { closeButton: true },
      );
    } catch (error) {
      toast.error(
        t("proxy.failoverQueue.addFailed", "添加失败") + ": " + String(error),
      );
    }
  };

  // 从队列移除供应商
  const handleRemoveProvider = async (providerId: string) => {
    try {
      await removeFromQueue.mutateAsync({ appType, providerId });
      toast.success(
        t("proxy.failoverQueue.removeSuccess", "已从故障转移队列移除"),
        { closeButton: true },
      );
    } catch (error) {
      toast.error(
        t("proxy.failoverQueue.removeFailed", "移除失败") +
          ": " +
          String(error),
      );
    }
  };

  const handleSetTier = async (providerId: string, tier: number) => {
    try {
      await setFailoverTier.mutateAsync({ appType, providerId, tier });
      toast.success(
        t("proxy.failoverQueue.setTierSuccess", "优先级已更新"),
        { closeButton: true },
      );
    } catch (error) {
      toast.error(
        t("proxy.failoverQueue.setTierFailed", "优先级更新失败") +
          ": " +
          String(error),
      );
    }
  };

  if (isQueueLoading) {
    return (
      <div className="flex items-center justify-center p-8">
        <Loader2 className="h-6 w-6 animate-spin text-muted-foreground" />
      </div>
    );
  }

  if (queueError) {
    return (
      <Alert variant="destructive">
        <AlertTriangle className="h-4 w-4" />
        <AlertDescription>{String(queueError)}</AlertDescription>
      </Alert>
    );
  }

  return (
    <div className="space-y-4">
      {/* 自动故障转移开关 */}
      <div className="flex items-center justify-between p-4 rounded-lg bg-muted/50 border border-border/50">
        <div className="space-y-0.5">
          <div className="flex items-center gap-2">
            <span className="text-sm font-medium">
              {t("proxy.failover.autoSwitch", {
                defaultValue: "自动故障转移",
              })}
            </span>
            {isFailoverEnabled && (
              <span className="px-2 py-0.5 text-xs rounded-full bg-emerald-500/20 text-emerald-600 dark:text-emerald-400">
                {t("common.enabled", { defaultValue: "已开启" })}
              </span>
            )}
          </div>
          <p className="text-xs text-muted-foreground">
            {t("proxy.failover.autoSwitchDescription", {
              defaultValue:
                "开启后优先使用 P1；只有 P1 全部不可用/失败后才降级到 P2（以此类推）",
            })}
          </p>
        </div>
        <Switch
          checked={isFailoverEnabled}
          onCheckedChange={handleToggleFailover}
          disabled={disabled || setFailoverEnabled.isPending}
        />
      </div>

      {/* 说明信息 */}
      <Alert className="border-blue-500/40 bg-blue-500/10">
        <Info className="h-4 w-4" />
        <AlertDescription className="text-sm">
          {t(
            "proxy.failoverQueue.info",
            "系统会按优先级层级（P1 → P2 → …）进行故障转移；同一层可配置多个供应商。",
          )}
        </AlertDescription>
      </Alert>

      {/* 添加供应商 */}
      <div className="flex items-center gap-2">
        <Select
          value={selectedProviderId}
          onValueChange={setSelectedProviderId}
          disabled={disabled || isProvidersLoading}
        >
          <SelectTrigger className="flex-1">
            <SelectValue
              placeholder={t(
                "proxy.failoverQueue.selectProvider",
                "选择供应商添加到队列",
              )}
            />
          </SelectTrigger>
          <SelectContent>
            {availableProviders?.map((provider) => (
              <SelectItem key={provider.id} value={provider.id}>
                {provider.name}
                {provider.notes && (
                  <span className="ml-1 text-xs text-muted-foreground">
                    ({provider.notes})
                  </span>
                )}
              </SelectItem>
            ))}
            {(!availableProviders || availableProviders.length === 0) && (
              <div className="px-2 py-4 text-center text-sm text-muted-foreground">
                {t(
                  "proxy.failoverQueue.noAvailableProviders",
                  "没有可添加的供应商",
                )}
              </div>
            )}
          </SelectContent>
        </Select>

        <Select
          value={selectedTier}
          onValueChange={setSelectedTier}
          disabled={disabled}
        >
          <SelectTrigger className="w-[4.25rem]">
            <SelectValue placeholder="P1" />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="1">P1</SelectItem>
            <SelectItem value="2">P2</SelectItem>
            <SelectItem value="3">P3</SelectItem>
          </SelectContent>
        </Select>

        <Button
          onClick={handleAddProvider}
          disabled={
            disabled ||
            !selectedProviderId ||
            addToQueue.isPending ||
            setFailoverTier.isPending
          }
          size="icon"
          variant="outline"
        >
          {addToQueue.isPending ? (
            <Loader2 className="h-4 w-4 animate-spin" />
          ) : (
            <Plus className="h-4 w-4" />
          )}
        </Button>
      </div>

      {/* 队列列表 */}
      {!queue || queue.length === 0 ? (
        <div className="rounded-lg border border-dashed border-muted-foreground/40 p-8 text-center">
          <p className="text-sm text-muted-foreground">
            {t(
              "proxy.failoverQueue.empty",
              "故障转移队列为空。添加供应商以启用自动故障转移。",
            )}
          </p>
        </div>
      ) : (
        <div className="space-y-2">
          {queue.map((item) => (
            <QueueItem
              key={item.providerId}
              item={item}
              disabled={disabled}
              onRemove={handleRemoveProvider}
              onSetTier={handleSetTier}
              isRemoving={removeFromQueue.isPending}
              isUpdatingTier={setFailoverTier.isPending}
              metrics={metricsByProvider.get(item.providerId)}
            />
          ))}
        </div>
      )}

      {/* 队列说明 */}
      {queue && queue.length > 0 && (
        <p className="text-xs text-muted-foreground">
          {t(
            "proxy.failoverQueue.orderHint",
            "同一优先级内的顺序与首页供应商列表顺序一致，可在首页拖拽调整。",
          )}
        </p>
      )}
    </div>
  );
}

interface QueueItemProps {
  item: FailoverQueueItem;
  disabled: boolean;
  onRemove: (providerId: string) => void;
  onSetTier: (providerId: string, tier: number) => void;
  isRemoving: boolean;
  isUpdatingTier: boolean;
  metrics?: ProviderHealthMetricsView;
}

function QueueItem({
  item,
  disabled,
  onRemove,
  onSetTier,
  isRemoving,
  isUpdatingTier,
  metrics,
}: QueueItemProps) {
  const { t } = useTranslation();

  return (
    <div
      className={cn(
        "flex items-center gap-3 rounded-lg border bg-card p-3 transition-colors",
      )}
    >
      {/* 优先级 */}
      <div className="flex h-6 w-6 shrink-0 items-center justify-center rounded-full bg-muted text-xs font-medium">
        P{item.failoverTier ?? 1}
      </div>

      {/* 供应商名称 + 指标徽章 */}
      <div className="flex-1 min-w-0">
        <div className="flex items-center gap-2 flex-wrap">
          <span className="text-sm font-medium truncate">
            {item.providerName}
            {item.providerNotes && (
              <span className="ml-1 text-xs text-muted-foreground">
                ({item.providerNotes})
              </span>
            )}
          </span>
          <ProviderMetricsBadges metrics={metrics} />
        </div>
      </div>

      {/* Tier 选择 + 删除按钮 */}
      <Select
        value={String(item.failoverTier ?? 1)}
        onValueChange={(value) => onSetTier(item.providerId, Number(value))}
        disabled={disabled || isUpdatingTier}
      >
        <SelectTrigger className="h-8 w-[4.25rem]">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          <SelectItem value="1">P1</SelectItem>
          <SelectItem value="2">P2</SelectItem>
          <SelectItem value="3">P3</SelectItem>
        </SelectContent>
      </Select>

      <Button
        variant="ghost"
        size="icon"
        className="h-8 w-8 shrink-0 text-muted-foreground hover:text-destructive"
        onClick={() => onRemove(item.providerId)}
        disabled={disabled || isRemoving}
        aria-label={t("common.delete", "删除")}
      >
        {isRemoving ? (
          <Loader2 className="h-4 w-4 animate-spin" />
        ) : (
          <Trash2 className="h-4 w-4" />
        )}
      </Button>
    </div>
  );
}

/**
 * Provider 指标徽章：缓存命中率 / 假 200 率 / 首字节延迟
 *
 * 没有样本（`metrics` 为 undefined 或 totalRequests=0）时显示"—"占位，
 * 避免 UI 里留白让用户误以为坏了。
 */
function ProviderMetricsBadges({
  metrics,
}: {
  metrics?: ProviderHealthMetricsView;
}) {
  const { t } = useTranslation();

  const hasData = !!metrics && metrics.totalRequests > 0;

  if (!hasData) {
    return (
      <span className="text-[10px] text-muted-foreground/70 font-mono">
        {t("proxy.failoverQueue.metrics.noData", "暂无近 30 分钟样本")}
      </span>
    );
  }

  const m = metrics!;
  return (
    <div className="flex items-center gap-1 flex-wrap">
      <MetricBadge
        label={t("proxy.failoverQueue.metrics.cacheHit", "缓存命中")}
        value={formatPercent(m.cacheHitRate)}
        tone={cacheHitTone(m.cacheHitRate)}
        title={t(
          "proxy.failoverQueue.metrics.cacheHitHint",
          [
            "按 token 加权：cache_read / 总 prompt tokens。",
            "用于横向对比 provider 的缓存行为（不是美元节省率）。",
            "低于 20% 通常意味着该 provider 不做缓存，或 prompt 不利于缓存。",
          ].join("\n"),
        )}
      />
      <MetricBadge
        label={t("proxy.failoverQueue.metrics.fake200", "断流率")}
        value={formatPercent(m.fake200Rate)}
        tone={fake200Tone(m.fake200Rate)}
        title={t(
          "proxy.failoverQueue.metrics.fake200Hint",
          "流式 200 响应里没有终止事件的比例；过高说明上游在中途关流",
        )}
      />
      <MetricBadge
        label="TTFT"
        value={formatTtft(m.avgFirstTokenMs)}
        tone={ttftTone(m.avgFirstTokenMs)}
        title={t(
          "proxy.failoverQueue.metrics.ttftHint",
          "平均首字节延迟；数值偏大说明该 provider 响应慢",
        )}
      />
      <span
        className="text-[10px] text-muted-foreground/60 font-mono"
        title={t(
          "proxy.failoverQueue.metrics.sampleSizeHint",
          "最近 30 分钟内的成功 + 失败请求总数",
        )}
      >
        n={m.totalRequests}
      </span>
    </div>
  );
}

type Tone = "good" | "warn" | "bad" | "neutral";

function MetricBadge({
  label,
  value,
  tone,
  title,
}: {
  label: string;
  value: string;
  tone: Tone;
  title?: string;
}) {
  const toneClass = {
    good: "bg-emerald-500/15 text-emerald-600 dark:text-emerald-400",
    warn: "bg-amber-500/15 text-amber-600 dark:text-amber-400",
    bad: "bg-red-500/15 text-red-600 dark:text-red-400",
    neutral: "bg-muted text-muted-foreground",
  }[tone];

  return (
    <span
      className={cn(
        "inline-flex items-center gap-0.5 rounded px-1.5 py-0.5 text-[10px] font-mono",
        toneClass,
      )}
      title={title}
    >
      <span className="opacity-70">{label}</span>
      <span className="font-medium">{value}</span>
    </span>
  );
}

function formatPercent(rate: number | null): string {
  if (rate === null || Number.isNaN(rate)) return "—";
  return `${(rate * 100).toFixed(1)}%`;
}

function formatTtft(ms: number | null): string {
  if (ms === null) return "—";
  if (ms < 1000) return `${ms}ms`;
  return `${(ms / 1000).toFixed(1)}s`;
}

function cacheHitTone(rate: number | null): Tone {
  if (rate === null) return "neutral";
  if (rate >= 0.7) return "good";
  if (rate >= 0.3) return "warn";
  return "bad";
}

function fake200Tone(rate: number | null): Tone {
  if (rate === null) return "neutral";
  if (rate <= 0.02) return "good";
  if (rate <= 0.1) return "warn";
  return "bad";
}

function ttftTone(ms: number | null): Tone {
  if (ms === null) return "neutral";
  if (ms <= 2000) return "good";
  if (ms <= 5000) return "warn";
  return "bad";
}
