type Request = (path: string, options?: { method?: string; body?: object; quiet?: boolean }) => Promise<any>;

type Revision = {
  id: string;
  kind: 'original' | 'correction' | 'retranscription';
  state: 'pending' | 'current' | 'superseded';
  raw_text: string;
  processed_text: string | null;
  final_text: string;
  asr_model: string | null;
  asr_provider: string | null;
  device: string | null;
};

type Model = { id: string; name: string; installed: boolean; kind: string; in_use: boolean };

export async function offlineModels(api: Request): Promise<Model[]> {
  const models: Model[] = await api('/api/models');
  return models.filter(model => model.installed && ['gguf', 'sherpa'].includes(model.kind));
}

export function historyRevisions(api: Request, id: string) {
  const record = `/api/records/${encodeURIComponent(id)}`;
  const revisions = `${record}/revisions`;
  return {
    async list(): Promise<Revision[]> { return (await api(revisions, { quiet: true })).revisions; },
    correct(text: string, expected: string) { return api(revisions, { method: 'POST', body: { text, expected } }); },
    recognize(model: string) { return api(`${record}/retranscribe`, { method: 'POST', body: { model } }); },
    adopt(version: string, expected: string) {
      return api(`${revisions}/${encodeURIComponent(version)}/adopt`, { method: 'POST', body: { expected } });
    },
  };
}
