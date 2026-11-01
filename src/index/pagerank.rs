//! Gewichteter PageRank über den Linkgraphen (CSR-Darstellung, 4 Byte pro Kante).
//! Links innerhalb derselben Domain zählen weniger als Links von fremden Domains –
//! das macht das Ranking robuster gegen seiteninterne Linkfarmen.

use rayon::prelude::*;

/// Kanten im CSR-Format. Das oberste Bit eines Ziels markiert einen domaininternen Link.
pub struct LinkGraph {
    pub offsets: Vec<u64>,
    pub targets: Vec<u32>,
}

pub const INTERNAL_BIT: u32 = 1 << 31;
const INTERNAL_WEIGHT: f32 = 0.3;

impl LinkGraph {
    pub fn new() -> LinkGraph {
        LinkGraph {
            offsets: vec![0],
            targets: Vec::new(),
        }
    }

    /// Fügt die (deduplizierten) ausgehenden Kanten des nächsten Dokuments hinzu.
    pub fn push_node(&mut self, targets: &[u32]) {
        self.targets.extend_from_slice(targets);
        self.offsets.push(self.targets.len() as u64);
    }

    pub fn num_nodes(&self) -> usize {
        self.offsets.len() - 1
    }

    pub fn num_edges(&self) -> usize {
        self.targets.len()
    }
}

impl Default for LinkGraph {
    fn default() -> Self {
        Self::new()
    }
}

/// Berechnet PageRank; Ergebnis ist mit N skaliert (Mittelwert 1.0).
pub fn pagerank(g: &LinkGraph, iterations: usize, damping: f32) -> Vec<f32> {
    let n = g.num_nodes();
    if n == 0 {
        return Vec::new();
    }
    let w = |t: u32| {
        if t & INTERNAL_BIT != 0 {
            INTERNAL_WEIGHT
        } else {
            1.0
        }
    };
    let out_w: Vec<f32> = (0..n)
        .map(|i| {
            g.targets[g.offsets[i] as usize..g.offsets[i + 1] as usize]
                .iter()
                .map(|&t| w(t))
                .sum()
        })
        .collect();
    let mut pr = vec![1.0f32; n];
    let mut next = vec![0.0f32; n];
    for _ in 0..iterations {
        next.iter_mut().for_each(|v| *v = 0.0);
        let mut dangling = 0.0f32;
        for i in 0..n {
            let (a, b) = (g.offsets[i] as usize, g.offsets[i + 1] as usize);
            if a == b || out_w[i] == 0.0 {
                dangling += pr[i];
                continue;
            }
            let share = pr[i] / out_w[i];
            for &t in &g.targets[a..b] {
                next[(t & !INTERNAL_BIT) as usize] += share * w(t);
            }
        }
        let base = (1.0 - damping) + damping * dangling / n as f32;
        next.par_iter_mut().for_each(|v| *v = base + damping * *v);
        std::mem::swap(&mut pr, &mut next);
    }
    pr
}

/// Bildet PageRank (Mittel 1.0) logarithmisch auf 0..=255 ab.
pub fn quantize(pr: f32) -> u8 {
    ((pr.max(1e-6).log2() + 2.0) * 20.0)
        .round()
        .clamp(0.0, 255.0) as u8
}

/// Statische Qualität 0..1 aus dem quantisierten Wert.
pub fn quality_score(q: u8) -> f32 {
    q as f32 / 255.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_graph() {
        // 1..4 zeigen auf 0, 0 zeigt auf 1
        let mut g = LinkGraph::new();
        g.push_node(&[1]);
        for _ in 1..5 {
            g.push_node(&[0]);
        }
        let pr = pagerank(&g, 40, 0.85);
        assert!(pr[0] > pr[1] && pr[1] > pr[2]);
        let sum: f32 = pr.iter().sum();
        assert!((sum - 5.0).abs() < 0.01, "{sum}");
        assert!(quantize(pr[0]) > quantize(pr[2]));
    }
}
