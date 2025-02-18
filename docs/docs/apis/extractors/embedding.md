# Embedding Extractors

#### [OpenAI Clip](https://github.com/tensorlakeai/indexify-extractors/tree/main/embedding/clip_embedding)
This extractor utilizes OpenAI's CLIP model to generate embeddings for both images and text.

#### [ColBERT](https://github.com/tensorlakeai/indexify-extractors/tree/main/embedding/colbert)
This ColBERTv2-based extractor is a Python class that encapsulates the functionality to convert text inputs into vector embeddings using the ColBERTv2 model. It leverages ColBERTv2's transformer-based architecture to generate context-aware embeddings suitable for various natural language processing tasks.

#### [E5](https://github.com/tensorlakeai/indexify-extractors/tree/main/embedding/e5_embedding)
A good small and fast general model for similarity search or downstream enrichments.
Based on [E5_Small_V2](https://huggingface.co/intfloat/e5-small-v2) which only works for English texts. Long texts will be truncated to at most 512 tokens.

#### [Hash](https://github.com/tensorlakeai/indexify-extractors/tree/main/embedding/hash-embedding)
This extractor extractors an "identity-"embedding for a piece of text, or file. It uses the sha256 to calculate the unique embeding for a given text, or file. This can be used to quickly search for duplicates within a large set of data.

#### [Jina](https://github.com/tensorlakeai/indexify-extractors/tree/main/embedding/jina_base_en)
This extractor extractors an embedding for a piece of text.
It uses the huggingface [Jina model](https://huggingface.co/jinaai/jina-embeddings-v2-base-en) which is an English, monolingual embedding model supporting 8192 sequence length. It is based on a Bert architecture (JinaBert) that supports the symmetric bidirectional variant of ALiBi to allow longer sequence length.

#### [Arctic - Sentence Transformer](https://github.com/tensorlakeai/indexify-extractors/tree/main/embedding/arctic)
This extractor extractors an embedding for a piece of text.
It uses the huggingface [Snowflake's Arctic-embed-m](https://huggingface.co/Snowflake/snowflake-arctic-embed-m). The snowflake-arctic-embedding models achieve state-of-the-art performance on the MTEB/BEIR leaderboard for each of their size variants.

#### [MiniLML6 - Sentence Transformer](https://github.com/tensorlakeai/indexify-extractors/tree/main/embedding/minilm-l6)
This extractor extractors an embedding for a piece of text.
It uses the huggingface [MiniLM-6 model](https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2), which is a tiny but very robust emebdding model for text.

#### [MPnet - Sentence Transformer](https://github.com/tensorlakeai/indexify-extractors/tree/main/embedding/mpnet)
This is a sentence embedding extractor based on the [MPNET Multilingual Base V2](https://huggingface.co/sentence-transformers/paraphrase-multilingual-mpnet-base-v2).
This is a sentence-transformers model: It maps sentences & paragraphs to a 768 dimensional dense vector space and can be used for tasks like clustering or semantic search.
It's best use case is paraphrasing, but it can also be used for other tasks.

#### [OpenAI Embedding](https://github.com/tensorlakeai/indexify-extractors/tree/main/embedding/openai-embedding)
This extractor extracts an embedding for a piece of text.
It uses the OpenAI text-embedding-ada-002 model.

#### [SciBERT Uncased](https://github.com/tensorlakeai/indexify-extractors/tree/main/embedding/scibert)
This is the pretrained model presented in [SciBERT: A Pretrained Language Model for Scientific Text](https://www.aclweb.org/anthology/D19-1371/), which is a BERT model trained on scientific text.
Works best with scientific text embedding extraction.

