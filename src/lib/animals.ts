// Animaux low-poly de l'univers Lucania, viewBox 0 0 64 64.
// Format : facets = polygones { points, opacity } ; eyes = cercles { cx, cy, r } ;
// accents = petits triangles { points }. Tout est rendu en currentColor (aucun rouge).
// hibou, corbeau et cerf sont copiés à l'identique de Lucania ; l'ours est propre à Ursa.
//
// Table des rôles :
//   ours    = logo + mascotte (états vides, bandeau de mise à jour)
//   hibou   = vérification / rapport d'intégrité
//   corbeau = Journal
//   cerf    = Réglages
type Point = [number, number];
export interface Animal {
  id: string;
  set: string;
  facets: { points: Point[]; opacity: number }[];
  eyes: { cx: number; cy: number; r: number }[];
  accents: { points: Point[] }[];
}

export const animals: Record<string, Animal> = {
  ours: {
    id: 'ours',
    set: 'minimal',
    facets: [
      { points: [[8,10],[20,5],[25,15],[14,19]], opacity: 0.7 },
      { points: [[56,10],[44,5],[39,15],[50,19]], opacity: 0.5 },
      { points: [[25,15],[39,15],[32,30]], opacity: 0.85 },
      { points: [[14,19],[25,15],[32,30],[9,32]], opacity: 0.6 },
      { points: [[50,19],[39,15],[32,30],[55,32]], opacity: 0.4 },
      { points: [[32,30],[40,38],[24,38]], opacity: 0.35 },
      { points: [[9,32],[32,30],[24,38],[21,50],[18,52]], opacity: 0.5 },
      { points: [[55,32],[32,30],[40,38],[43,50],[46,52]], opacity: 0.3 },
      { points: [[24,38],[40,38],[43,50],[32,57],[21,50]], opacity: 0.75 },
      { points: [[18,52],[21,50],[32,57]], opacity: 0.45 },
      { points: [[46,52],[43,50],[32,57]], opacity: 0.3 },
    ],
    eyes: [{ cx: 21, cy: 28, r: 1.8 }, { cx: 43, cy: 28, r: 1.8 }],
    accents: [{ points: [[28,38],[36,38],[32,44]] }],
  },
  corbeau: {
    id: 'corbeau',
    set: 'minimal',
    facets: [
      { points: [[33,19],[38,9],[47,5],[52,12],[49,14]], opacity: 0.85 },
      { points: [[33,19],[49,14],[49,24],[41,27]], opacity: 1 },
      { points: [[49,14],[63,28],[49,24]], opacity: 0.5 },
      { points: [[49,24],[63,28],[47,31]], opacity: 0.3 },
      { points: [[41,27],[49,24],[47,31],[52,42],[44,50]], opacity: 0.45 },
      { points: [[33,19],[41,27],[12,30]], opacity: 0.45 },
      { points: [[12,30],[41,27],[44,50],[30,54]], opacity: 0.7 },
      { points: [[12,30],[30,54],[3,61]], opacity: 0.3 },
      { points: [[12,30],[3,61],[4,40]], opacity: 0.5 },
    ],
    eyes: [{ cx: 43, cy: 18.5, r: 2 }],
    accents: [],
  },
  hibou: {
    id: 'hibou',
    set: 'minimal',
    facets: [
      { points: [[10,8],[32,22],[32,58],[10,40]], opacity: 0.6 },
      { points: [[54,8],[32,22],[32,58],[54,40]], opacity: 0.35 },
      { points: [[28,32],[36,32],[32,44]], opacity: 1 },
    ],
    eyes: [{ cx: 21, cy: 28, r: 5 }, { cx: 43, cy: 28, r: 5 }],
    accents: [],
  },
  cerf: {
    id: 'cerf',
    set: 'minimal',
    facets: [
      { points: [[19,26],[8,4],[27,25]], opacity: 0.85 },
      { points: [[18,17],[4,14],[23,21]], opacity: 0.6 },
      { points: [[45,26],[56,4],[37,25]], opacity: 0.6 },
      { points: [[46,17],[60,14],[41,21]], opacity: 0.4 },
      { points: [[10,26],[22,26],[22,36]], opacity: 0.5 },
      { points: [[54,26],[42,26],[42,36]], opacity: 0.3 },
      { points: [[22,26],[42,26],[32,30]], opacity: 1 },
      { points: [[22,26],[32,30],[32,58]], opacity: 0.6 },
      { points: [[42,26],[32,30],[32,58]], opacity: 0.35 },
    ],
    eyes: [],
    accents: [],
  },
};

/** Renvoie les données d'un animal ; lève une Error si l'id est inconnu. */
export function getAnimal(id: string): Animal {
  const animal = animals[id];
  if (!animal) throw new Error(`Animal inconnu : ${id}`);
  return animal;
}
