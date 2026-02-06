from arango import ArangoClient
import matplotlib
import matplotlib.pyplot as plt


def query_blocks():
    # Initialize the ArangoDB client.
    client = ArangoClient()

    # Connect to "test" database as root user.
    db = client.db('blockchain', username='', password='')

    # Get the API wrapper for "students" collection.
    blocks = db.collection('blocks')

    length = len(blocks)

    # Get the IDs of all documents in the collection.
    cursor = blocks.keys()

    i = 0
    block_statistics = {}
    while True:
        try:
            key = cursor.next()
        except StopIteration:
            print("Finished loop ", i)
            break
        except Exception:
            print("Failed to get next key")
            exit(1)
        i += 1
        block = blocks.get(key)
        block_statistics[block['seq_no']] = block['tr_count']

    block_seq_nos = list(block_statistics.keys())
    block_seq_nos.sort()
    block_statistics = {i: block_statistics[i] for i in block_seq_nos}
    block_txns = list(block_statistics.values())

    font = {'size': 24}

    matplotlib.rc('font', **font)

    plt.plot(block_txns)
    plt.ylabel('Block statistics')
    plt.show()
    print("Finished")


if __name__ == "__main__":
    query_blocks()
